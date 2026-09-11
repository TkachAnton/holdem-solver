# Архитектура продвинутого NLHE 8-max MTT ChipEV Solver

## 0. Статус документа

Версия: 0.1

Целевой первый продукт: solver для No-Limit Texas Hold'em, 8-max, MTT, ChipEV.

ICM не входит в первый solver core как payoff-модель. Архитектура должна позволять добавить ICM отдельным модулем поверх корректно реализованной ChipEV-игры.

Основной режим исполнения: native Rust на локальной машине. WASM используется для интерфейсных и небольших локальных операций, но не является основным движком тяжёлого 8-max solving.

Размеры ставок: конфигурируемая action abstraction. Поддержка произвольного непрерывного размера ставки не является целью v1.

---

## 1. Продуктовая цель

Система должна уметь строить и решать сложные турнирные споты:

### Префлоп

- RFI из любой позиции;
- limp/open/iso-raise;
- действие против open raise: fold/call/3-bet/all-in;
- действие против cold call и squeeze;
- cold 4-bet и 5-bet;
- short-stack push/fold;
- разные эффективные стеки для каждого игрока;
- antes, blind level, button и порядок действий;
- 2–8 игроков в раздаче;
- несколько диапазонов с combo-level weights;
- узлы после конкретной истории действий.

### Постфлоп

- single-raised pots;
- 3-bet pots;
- 4-bet pots;
- heads-up и multiway;
- любой известный flop/turn/river;
- check/bet/raise/call/fold/all-in;
- несколько размеров ставок и рейзов;
- разные effective stacks;
- side pots и all-in-состояния;
- точный pot geometry;
- EV каждой ветки и action EV;
- стратегия по каждой комбинации или по заданному hand abstraction bucket.

### Аналитика

- частоты действий;
- EV каждой руки;
- EV действия относительно лучшего действия;
- aggregate EV диапазона;
- range matrix 13x13;
- combo view с блокерами;
- стратегия по конкретному decision node;
- сравнение двух решений;
- approximate exploitability / best response;
- checkpoint/resume долгого расчёта;
- экспорт стратегии в JSON/CSV/bin.

---

## 2. Ограничения и честные ожидания

### 2.1. Нельзя решить весь 8-max NLHE без абстракций

Полный game tree произвольной игры с:

- 8 игроками;
- глубокими стеками;
- неограниченными bet sizes;
- всеми turn/river runout;
- всеми exact combos;

экспоненциально велик.

Поэтому продукт должен разделять:

1. точную модель правил;
2. точное состояние конкретного spot;
3. configurable action abstraction;
4. card/private-hand abstraction;
5. sampling algorithm;
6. кэш и библиотеку заранее решённых решений.

Цель — не делать вид, что это continuous exact Nash solver, а явно показывать пользователю:

- какие размеры действий использовались;
- сколько итераций выполнено;
- какая abstraction применена;
- какой estimated regret/exploitability получен;
- насколько решение стабильно при смене seed.

### 2.2. ChipEV и ICM — разные уровни

ChipEV использует фишки как линейную utility:

```text
utility = chips won - chips invested
```

ICM зависит от:

- payout structure;
- stack distribution за столами;
- числа оставшихся игроков;
- будущего tournament model;
- bust risk.

ICM нельзя реализовать только добавлением поля `icm: bool` в текущий CFR. Поэтому v1 реализует ChipEV через отдельный `ChipEvUtility`, а v2 добавляет `IcmUtility` через общий trait.

---

## 3. Главные архитектурные решения

### 3.1. Native solver как основной runtime

Тяжёлый solver работает в native Rust:

```text
solver-cli
solver-server (опциональный локальный HTTP/WebSocket server)
```

Браузер не должен синхронно запускать полный 8-max solve.

WASM используется для:

- card parser;
- range editor;
- быстрый hand evaluator;
- проверки легальности карт;
- небольших heads-up демонстраций;
- визуализации и клиентских вычислений.

### 3.2. Единая модель игры

Не должно быть независимых несовместимых реализаций:

```text
preflop_tree.rs
postflop_tree.rs
multiway_tree.rs
```

Должен быть один state machine и один action generator. Street и тип spot являются параметрами состояния.

### 3.3. Деньги — не f32

Все размеры банка и стеков хранятся в целых chip units:

```rust
pub type Chips = i64;
```

Например, можно использовать 1/1000 BB как единицу точности.

`f32` нельзя использовать для pot/invested/current_bet из-за ошибок сравнения и накопления.

Внутренние regret/EV значения хранятся в `f64`.

### 3.4. Нет одного representative на класс

169 preflop classes используются как abstraction information set, но equity/chance model работает с точными комбинациями:

```text
AA -> 6 exact combos
AKs -> 4 exact combos
AKo -> 12 exact combos
```

Стратегия может быть общей для класса, но конкретная раздача private cards должна быть точной и учитывать blockers.

### 3.5. Каждая стратегия привязана к decision node

Нельзя возвращать одну стратегию `AA` для всего дерева.

Корректный ключ:

```text
(node_id, player_id, private_hand_bucket)
```

История действий, street, pot, active players и current bet являются частью public state/node.

---

## 4. Предлагаемая структура проекта

```text
holdem-solver/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── LICENSE
├── docs/
│   ├── ARCHITECTURE_RU.md
│   ├── GAME_RULES.md
│   ├── CFR_NOTES.md
│   ├── RANGE_FORMAT.md
│   └── RESULT_SCHEMA.md
│
├── crates/
│   ├── cards/
│   │   ├── src/lib.rs
│   │   ├── src/card.rs
│   │   ├── src/deck.rs
│   │   └── tests/
│   │
│   ├── holdem-domain/
│   │   ├── src/lib.rs
│   │   ├── src/config.rs
│   │   ├── src/player.rs
│   │   ├── src/position.rs
│   │   ├── src/state.rs
│   │   ├── src/action.rs
│   │   ├── src/history.rs
│   │   ├── src/pot.rs
│   │   └── src/terminal.rs
│   │
│   ├── ranges/
│   │   ├── src/lib.rs
│   │   ├── src/parser.rs
│   │   ├── src/class.rs
│   │   ├── src/combo.rs
│   │   ├── src/weights.rs
│   │   └── src/blockers.rs
│   │
│   ├── evaluator/
│   │   ├── src/lib.rs
│   │   ├── src/eval5.rs
│   │   ├── src/eval7.rs
│   │   ├── src/tables.rs
│   │   └── benches/
│   │
│   ├── equity/
│   │   ├── src/lib.rs
│   │   ├── src/showdown.rs
│   │   ├── src/runouts.rs
│   │   ├── src/profile.rs
│   │   └── src/cache.rs
│   │
│   ├── tree/
│   │   ├── src/lib.rs
│   │   ├── src/builder.rs
│   │   ├── src/node.rs
│   │   ├── src/action_abstraction.rs
│   │   ├── src/preflop.rs
│   │   ├── src/postflop.rs
│   │   ├── src/chance.rs
│   │   └── src/validator.rs
│   │
│   ├── abstraction/
│   │   ├── src/lib.rs
│   │   ├── src/preflop_169.rs
│   │   ├── src/postflop_bucket.rs
│   │   ├── src/board_texture.rs
│   │   └── src/hand_bucket.rs
│   │
│   ├── solver/
│   │   ├── src/lib.rs
│   │   ├── src/infoset.rs
│   │   ├── src/store.rs
│   │   ├── src/cfr.rs
│   │   ├── src/cfr_plus.rs
│   │   ├── src/mccfr.rs
│   │   ├── src/best_response.rs
│   │   ├── src/exploitability.rs
│   │   ├── src/checkpoint.rs
│   │   └── src/progress.rs
│   │
│   ├── utility/
│   │   ├── src/lib.rs
│   │   ├── src/chip_ev.rs
│   │   ├── src/icm.rs
│   │   └── src/side_pots.rs
│   │
│   ├── storage/
│   │   ├── src/lib.rs
│   │   ├── src/result_store.rs
│   │   ├── src/equity_store.rs
│   │   └── src/formats.rs
│   │
│   ├── api/
│   │   ├── src/lib.rs
│   │   ├── src/request.rs
│   │   ├── src/response.rs
│   │   └── src/job.rs
│   │
│   └── wasm-bindings/
│       ├── src/lib.rs
│       └── Cargo.toml
│
├── apps/
│   ├── solver-cli/
│   │   └── src/main.rs
│   ├── solver-server/
│   │   └── src/main.rs
│   └── frontend/
│       ├── index.html
│       ├── app.js
│       ├── styles.css
│       └── wasm/
│
├── tests/
│   ├── cards_tests.rs
│   ├── equity_tests.rs
│   ├── tree_tests.rs
│   ├── chip_ev_tests.rs
│   ├── cfr_toy_games.rs
│   └── regression_tests.rs
│
└── benches/
    ├── evaluator.rs
    ├── equity.rs
    └── tree.rs
```

---

## 5. Domain model

### 5.1. Card model

```rust
pub type Card = u8; // 0..51

pub struct Board {
    pub cards: SmallVec<[Card; 5]>,
}

pub struct Combo {
    pub cards: [Card; 2],
}
```

Требования:

- duplicate cards запрещены;
- `Card` должен иметь быстрые rank/suit операции;
- deck mask должен использоваться для dead-card checks;
- порядок карт в combo должен быть canonical;
- board key должен быть canonical для cache.

### 5.2. Tournament configuration

```rust
pub struct TournamentConfig {
    pub table_size: usize,          // 2..8
    pub button_seat: usize,
    pub small_blind: Chips,
    pub big_blind: Chips,
    pub ante: Chips,
    pub ante_mode: AnteMode,
    pub straddle: Option<Chips>,
    pub blind_level_id: Option<String>,
}
```

ChipEV solver использует `small_blind`, `big_blind`, ante и текущие stacks. Payouts не участвуют в v1 utility.

### 5.3. Player state

```rust
pub struct PlayerState {
    pub seat: usize,
    pub stack_remaining: Chips,
    pub committed_total: Chips,
    pub committed_street: Chips,
    pub status: PlayerStatus,
    pub range: Option<WeightedRange>,
}

pub enum PlayerStatus {
    Active,
    Folded,
    AllIn,
    OutOfHand,
}
```

Отдельно хранятся:

- `committed_total` — сколько игрок вложил в текущую раздачу;
- `committed_street` — сколько вложил на текущей улице;
- `stack_remaining` — сколько ещё может поставить.

### 5.4. Public game state

```rust
pub struct GameState {
    pub street: Street,
    pub board: Board,
    pub players: Vec<PlayerState>,
    pub pot: Chips,
    pub actor: Option<usize>,
    pub current_bet: Chips,
    pub min_raise_increment: Chips,
    pub last_full_raise: Chips,
    pub pending_players: SeatMask,
    pub action_history: Vec<ActionRecord>,
    pub dead_money: Chips,
}
```

Инвариант:

```text
pot == dead_money + sum(player.committed_total)
```

с учётом уже уплаченной комиссии, если она включена в модель.

---

## 6. Единая модель действий

```rust
pub enum Action {
    Fold,
    Check,
    Call,
    Bet { amount: Chips },
    Raise { to: Chips },
    AllIn,
}
```

Не следует хранить просто `Action::Raise` без размера. Размер является частью действия и должен входить в public node/action key.

### 6.1. Legal action generator

```rust
pub trait LegalActions {
    fn legal_actions(&self, state: &GameState) -> Vec<Action>;
}
```

Один генератор используется для:

- preflop;
- flop;
- turn;
- river;
- heads-up;
- multiway;
- all-in/side-pot states.

### 6.2. Betting round

Каждая улица должна явно хранить:

- кто уже действовал;
- кто должен ответить на текущую ставку;
- target contribution;
- минимальный raise increment;
- кто может check/call/raise;
- когда улица завершена.

Нельзя выводить pending players только из сравнения `invested < bet_size`, потому что total investment и street investment — разные величины.

---

## 7. Action abstraction

Размеры ставок задаются конфигурацией:

```rust
pub struct ActionAbstraction {
    pub preflop: PreflopSizing,
    pub flop: StreetSizing,
    pub turn: StreetSizing,
    pub river: StreetSizing,
    pub include_all_in: bool,
    pub max_raises_per_street: usize,
}

pub struct StreetSizing {
    pub bet_fractions: Vec<f64>,
    pub raise_fractions: Vec<f64>,
    pub overbet_fractions: Vec<f64>,
    pub include_pot_size: bool,
    pub include_all_in: bool,
}
```

Пример конфигурации:

```json
{
  "preflop": {
    "open_sizes": [2.0, 2.2, 2.5, 3.0],
    "three_bet_multipliers": [2.5, 3.0, 3.5],
    "four_bet_multipliers": [2.1, 2.3, 2.5],
    "include_jam": true
  },
  "flop": {
    "bet_fractions": [0.25, 0.33, 0.5, 0.75, 1.0, 1.5],
    "raise_fractions": [2.5, 3.0],
    "include_all_in": true
  }
}
```

Все размеры проходят через legalizer:

- нельзя поставить больше stack;
- all-in превращается в отдельное действие;
- raise должен удовлетворять min-raise;
- размер округляется к chip unit;
- одинаковые действия после округления удаляются.

---

## 8. Range model

### 8.1. Weighted range

```rust
pub struct WeightedCombo {
    pub combo: Combo,
    pub class_id: HandClassId,
    pub weight: f64,
}

pub struct WeightedRange {
    pub combos: Vec<WeightedCombo>,
}
```

Диапазон поддерживает:

- `AA-77`;
- `AKs-A2s`;
- `AKo-AJo`;
- отдельные combo weights;
- исключённые карты;
- board filtering;
- frequency matrix;
- нормализацию после blocker filtering.

### 8.2. Combo probabilities

Вероятность combo определяется не только class:

```text
P(combo | range, dead_cards) ∝ combo.weight
```

После фильтрации board/dead cards веса ренормализуются.

Никаких фиксированных представителей вида `AsTs` для всего `ATs` не используется.

### 8.3. Preflop abstraction

Поддерживаются:

1. exact combo information sets;
2. 169 class information sets;
3. class + suit/blocker subtype;
4. пользовательские buckets.

В базовом режиме стратегия может быть общей для 169 класса, но equity считается по exact combo profile.

### 8.4. Postflop abstraction

Возможны режимы:

- exact combo;
- category bucket;
- draw/nut/blocker bucket;
- equity bucket относительно range;
- board texture bucket.

Каждый bucket должен быть versioned и детерминированным. Нельзя смешивать разные board textures без явного веса.

---

## 9. Tree model

### 9.1. Node types

```rust
pub enum NodeKind {
    Decision {
        actor: usize,
        actions: Vec<ActionId>,
    },
    Chance {
        outcomes: Vec<ChanceOutcome>,
    },
    Terminal {
        terminal: TerminalState,
    },
}
```

```rust
pub struct TreeNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
    pub public_state_hash: u128,
    pub history: ActionHistory,
}
```

`Chance` должен использовать `chance_weight`, а не усреднять детей одинаково без причины.

### 9.2. Preflop

Preflop tree строится тем же generic builder, но с особой betting configuration:

- position order от button;
- SB/BB/ante/straddle;
- open/limp/iso sizes;
- 3-bet/4-bet/5-bet;
- stack-dependent all-in;
- pending players после каждого raise;
- cold call/squeeze;
- side pots.

RFI, response to open, response to 3-bet и squeeze — не отдельные несовместимые tree implementations. Это разные public nodes одного дерева или subgame roots, полученные replay-ом action history.

### 9.3. Postflop

Postflop builder принимает уже сформированное состояние:

```rust
pub struct PostflopSpot {
    pub board: Board,
    pub players: Vec<PostflopPlayer>,
    pub pot: Chips,
    pub action_history: ActionHistory,
    pub street: Street,
    pub abstraction: ActionAbstraction,
}
```

Поддерживаются:

- SRP;
- 3BP;
- 4BP;
- single-raised multiway;
- check-through;
- bet/call;
- bet/raise;
- all-in;
- turn/river chance nodes.

### 9.4. Spot replay

Пользователь может задать:

```text
- starting configuration;
- action history;
- current board;
- current pot/stacks;
```

Система сначала replay-ит историю через общий state machine, затем валидирует, что полученное состояние легально.

---

## 10. Equity engine

### 10.1. Один evaluator

Необходимо оставить одну реализацию оценки руки для всего проекта:

```rust
pub trait HandEvaluator {
    fn evaluate_5(&self, cards: [Card; 5]) -> HandValue;
    fn evaluate_7(&self, cards: [Card; 7]) -> HandValue;
    fn evaluate_partial(&self, cards: &[Card]) -> HandValue;
}
```

Тесты должны покрывать 5, 6 и 7 cards. Текущая реализация `evaluate_hand`, которая фактически работает только при `n == 7`, должна быть заменена.

### 10.2. Profile equity

```rust
pub struct EquityProfile {
    pub players: Vec<Combo>,
    pub active_players: SeatMask,
    pub board: Board,
}

pub struct EquityResult {
    pub shares: Vec<f64>,
    pub wins: Vec<u64>,
    pub ties: Vec<u64>,
    pub runouts: u64,
}
```

При расчёте:

- все hole cards считаются dead cards, включая folded players;
- showdown участвуют только active players;
- ties распределяются по pot/side-pot правилам;
- active mask входит в cache key;
- exact board/runout enumeration используется там, где возможно;
- MC/stratified sampling используется только с явным quality metric.

### 10.3. Equity cache key

Минимальный ключ:

```text
version
board
all exact hole cards
active mask
side-pot mask
runout policy
```

Нельзя кэшировать equity только по:

```text
class_id + board
```

если exact combo/blocker model ещё не реализован.

### 10.4. Profile equity одним вызовом

Не следует вызывать equity calculator отдельно для каждого игрока, пересчитывая одни и те же runouts. Нужно считать один profile showdown и вернуть shares всех активных игроков.

---

## 11. Utility layer

### 11.1. ChipEV

```rust
pub trait UtilityModel {
    fn terminal_utilities(
        &self,
        terminal: &TerminalState,
        showdown: Option<&EquityResult>,
    ) -> Vec<f64>;
}
```

```rust
pub struct ChipEvUtility {
    pub rake: Chips,
}
```

ChipEV payoff:

```text
utility_i = payout_i - committed_total_i
```

Для folded players payout равен нулю, но уже вложенные chips остаются sunk cost.

### 11.2. ICM extension

В будущем:

```rust
pub struct IcmUtility {
    pub payouts: Vec<f64>,
    pub remaining_stacks: Vec<Chips>,
    pub table_context: TournamentTableContext,
}
```

ICM будет отдельным utility backend, а не логикой внутри tree builder или CFR.

---

## 12. Solver architecture

### 12.1. Information set

```rust
pub struct InfoSetKey {
    pub node_id: NodeId,
    pub player: usize,
    pub private_bucket: PrivateBucketId,
}
```

В зависимости от режима `private_bucket` может быть:

- exact combo;
- 169 class;
- postflop hand bucket;
- combo + blocker subtype.

### 12.2. Store

```rust
pub struct InfoSetData {
    pub regrets: Vec<f64>,
    pub strategy_sum: Vec<f64>,
    pub visits: u64,
    pub weighted_visits: f64,
    pub last_update: u64,
}
```

Store индексируется напрямую по `node_id`/`infoset_id`, а не через постоянный:

```rust
node_cfr.iter().find(...)
```

### 12.3. Heads-up

Для HU использовать:

- CFR+;
- DCFR как альтернативу;
- full traversal для небольших деревьев;
- chance sampling для больших postflop trees;
- best response/exploitability validation.

### 12.4. Multiway

Для 3–8 игроков использовать:

- external-sampling MCCFR;
- public chance sampling;
- exact combo sampling с blocker-aware probabilities;
- deterministic per-worker RNG;
- separate regret update and average-strategy update;
- player-specific traverser iteration или round-robin traverser.

В full-tree CFR:

```text
regret update uses π_-i
average strategy uses π_i
```

В sampled CFR дополнительно применяется importance correction.

### 12.5. Multiplayer result semantics

Для 3+ игроков результат должен содержать:

- average counterfactual regret;
- per-player best-response estimate;
- NashConv estimate, если возможно;
- confidence/variance estimate;
- strategy stability across seeds.

Нельзя сообщать пользователю, что произвольный multiway solve является точным Nash equilibrium после фиксированных 500 итераций.

### 12.6. Checkpointing

Checkpoint должен хранить:

```text
solver version
config hash
tree hash
abstraction hash
rng seed
iteration
regret store
strategy store
metrics
```

После изменения дерева или abstraction старый checkpoint должен быть несовместим.

---

## 13. Performance strategy

### 13.1. Не перечислять декартов продукт всех диапазонов

Текущий подход:

```text
range_0 × range_1 × ... × range_7
```

не масштабируется.

Для multiway:

- сэмплировать exact profile;
- использовать external sampling;
- не создавать `Vec<Vec<[u8; 2]>>` для всего продукта;
- использовать callback/iterator/stack buffer;
- отдельно контролировать sampling probability.

### 13.2. Profile equity batching

Для одного exact profile можно одновременно считать:

- shares всех active players;
- side-pot payouts;
- hand categories;
- runout statistics.

### 13.3. Parallelism

На native runtime:

- Rayon worker pool;
- отдельный solver worker на задачу;
- per-thread regret buffers;
- deterministic reduction;
- checkpoint между batches;
- ограничение памяти через cache budget.

Не использовать на первом этапе shared mutable `HashMap` из нескольких потоков без понятной модели reduction.

### 13.4. Cache layers

1. Card/evaluator lookup tables.
2. Range parsing/legalization cache.
3. Exact profile equity cache.
4. Public-node tree cache.
5. Solver checkpoint/result cache.

Все cache keys versioned.

---

## 14. Native API и локальные jobs

Даже при локальном запуске удобнее иметь `solver-server`, чтобы frontend не блокировался.

### 14.1. CLI

```bash
holdem-solver validate-spot spot.json
holdem-solver build-tree spot.json --out tree.bin
holdem-solver solve spot.json --out solution.bin
holdem-solver resume solution.bin
holdem-solver analyze solution.bin --node 42
holdem-solver best-response solution.bin
```

### 14.2. Local server

```text
POST /api/v1/jobs
GET  /api/v1/jobs/:id
GET  /api/v1/jobs/:id/progress
GET  /api/v1/jobs/:id/tree
GET  /api/v1/jobs/:id/node/:node_id/strategy
POST /api/v1/jobs/:id/checkpoint
POST /api/v1/jobs/:id/cancel
```

Progress:

```json
{
  "job_id": "...",
  "status": "running",
  "iteration": 12000,
  "target_iterations": 100000,
  "nodes": 18342,
  "infosets": 948120,
  "regret": 0.031,
  "cache_hit_rate": 0.94,
  "throughput": 4210.2
}
```

### 14.3. Result format

Не передавать все strategies огромным JSON одним ответом. Использовать:

- binary snapshot для хранения;
- paginated node queries;
- JSON только для выбранного node/class;
- CSV export для range charts.

---

## 15. Frontend

Frontend остаётся на HTML + JS, но должен быть node-oriented.

### Основные экраны

1. **Tournament setup**
   - seats;
   - positions;
   - stacks;
   - blinds/ante;
   - button;
   - MTT ChipEV context.

2. **Spot builder**
   - action history;
   - board picker;
   - pot/effective stacks;
   - range editor для каждого игрока;
   - bet/raise sizes.

3. **Tree explorer**
   - decision nodes;
   - action history;
   - active players;
   - pot and SPR;
   - node selection.

4. **Strategy viewer**
   - 13x13 matrix;
   - exact combo grid;
   - action frequencies;
   - action EV;
   - EV loss relative to best action;
   - aggregate range frequency.

5. **Solve monitor**
   - progress;
   - throughput;
   - regret;
   - cache hit rate;
   - checkpoint/resume;
   - seed comparison.

6. **Comparison/trainer**
   - compare two strategies;
   - identify mistakes;
   - drill selected node;
   - show recommended action and EV penalty.

Frontend не должен предполагать, что `hand_name` глобально уникален без `node_id`.

---

## 16. Testing strategy

### 16.1. Cards/evaluator

- 52 unique cards;
- card encode/decode roundtrip;
- royal flush;
- wheel;
- flush vs straight;
- full house vs flush;
- 5-card, 6-card, 7-card evaluation;
- duplicate-card rejection.

### 16.2. Ranges

- class counts;
- AA = 6;
- AKs = 4;
- AKo = 12;
- weighted combo parsing;
- board filtering;
- blocker renormalization;
- range union/intersection.

### 16.3. Game state

- pot conservation;
- actor order;
- pending responders;
- min-raise;
- all-in;
- side pots;
- action history replay;
- preflop position order;
- postflop position order;
- heads-up bet has fold/call response;
- 3-way bet asks the correct players in the correct order.

### 16.4. Equity

- exact known hand matchups;
- ties;
- active mask;
- folded cards as dead cards;
- side-pot equity;
- cache key differs for different active sets;
- cache key differs for different exact combos.

### 16.5. CFR toy games

До запуска покерного solver нужно протестировать CFR на маленьких games с известным equilibrium:

- Kuhn Poker;
- Leduc Poker;
- toy betting game;
- маленький 2-player all-in game;
- маленький 3-player game для проверки regret updates.

### 16.6. Regression tests

Smoke tests вроде AA/KK полезны, но недостаточны. Они должны сопровождаться:

- action EV;
- root regret;
- visits;
- exploitability estimate;
- fixed seed;
- expected tree hash.

---

## 17. План реализации

### Этап 0. Спецификация и clean foundation

Результат:

- Cargo workspace;
- versioned schemas;
- `SpotConfig`;
- `TournamentConfig`;
- базовые error types;
- deterministic seed model;
- архитектурные тесты.

Текущий код `multiway_tree.rs` и `multiway_cfr.rs` не переносится напрямую. Он используется как источник требований и regression cases.

### Этап 1. Cards, ranges, evaluator, exact equity

Результат:

- единый evaluator;
- exact combos;
- weighted ranges;
- blocker filtering;
- profile equity;
- active/folded/dead card correctness;
- cache key tests.

### Этап 2. Generic game state и legal action engine

Результат:

- preflop/postflop state machine;
- position/order model;
- blinds/ante/straddle;
- stack/all-in/side-pot logic;
- action history replay;
- tree validator.

### Этап 3. Heads-up ChipEV solver

Результат:

- preflop HU;
- postflop HU;
- configurable sizes;
- CFR+;
- average strategy;
- action EV;
- checkpoint;
- exploitability tests on toy games.

### Этап 4. Multiway preflop 3–8 players

Результат:

- exact private-card sampling;
- external-sampling MCCFR;
- 3–8 players;
- RFI/open/3-bet/4-bet/squeeze;
- short-stack all-ins;
- per-node strategy output;
- variance and seed metrics.

### Этап 5. Multiway postflop

Результат:

- flop/turn/river chance nodes;
- multiway check/bet/raise/call;
- side pots;
- postflop action abstraction;
- board texture abstraction;
- profile equity batching.

### Этап 6. Native CLI/server + frontend

Результат:

- local job manager;
- progress WebSocket;
- result snapshots;
- tree explorer;
- range matrix;
- strategy/EV viewer;
- CSV/JSON export.

### Этап 7. Performance and solution library

Результат:

- Rayon workers;
- compact stores;
- cache eviction;
- precomputed common MTT spots;
- solution library by stack/blind/position/action abstraction;
- fast lookup mode.

### Этап 8. ICM extension

Результат:

- payout model;
- table/remaining players model;
- ICM utility;
- ChipEV vs ICM comparison;
- tournament stage configuration;
- risk-premium analysis.

---

## 18. Acceptance criteria первого качественного релиза

Первый серьёзный релиз нельзя считать готовым только потому, что AA чаще рейзит.

Минимальные критерии:

1. Все legal actions корректны для HU и 3-way.
2. Pot/stack/side-pot invariants проходят property tests.
3. Equity совпадает с эталонными результатами.
4. Folded cards никогда не попадают в runout.
5. Стратегия возвращается по конкретному decision node.
6. У каждой стратегии есть visits и convergence metrics.
7. Нет неявного равного деления банка при cache miss.
8. Exact combo weights учитывают blockers.
9. Board/chance sampling имеет зафиксированную probability model.
10. HU toy games сходятся к известному equilibrium.
11. Multiway output содержит approximate regret/NashConv metrics.
12. Solver можно остановить и продолжить из checkpoint.
13. Результат воспроизводим при фиксированном seed.
14. Frontend показывает используемую abstraction.
15. ChipEV и будущий ICM не смешаны в одном неявном utility расчёте.

---

## 19. Что переносится из текущего проекта

Можно сохранить и переработать:

- кодировку карт 0–51;
- card string parser;
- базовые range parser tests;
- идеи 169 class abstraction;
- тесты hand categories;
- WASM ping и небольшие интерфейсные функции;
- формат frontend strategy chart.

Не следует переносить без переписывания:

- `MultiwayTreeBuilder`;
- `build_response_to_bet`;
- `multiway_cfr::solve`;
- representative-only sampling;
- `calculate_hand_ev`;
- duplicate evaluator из `equity.rs`;
- global cache без versioned keys;
- output через `first_node`.

---

## 20. Первый практический deliverable

Первым кодовым этапом после утверждения этой архитектуры должен быть не frontend и не optimisation.

Нужно реализовать вертикальный срез:

```text
cards
→ exact weighted ranges
→ unified evaluator
→ generic HU game state
→ legal action tree
→ exact equity
→ tiny CFR+ solver
→ node-specific result
→ regression tests
```

После того как этот срез будет корректен, его можно расширить до:

```text
HU postflop
→ 3-way preflop MCCFR
→ 8-max preflop
→ multiway postflop
→ native jobs/frontend
→ ICM
```

Это даст устойчивый фундамент и не позволит снова получить ситуацию, когда интерфейс показывает красивые проценты, но они относятся к неправильному узлу, неправильному active set или неправильному pot accounting.
