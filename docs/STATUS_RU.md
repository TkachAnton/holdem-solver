# Статус разработки

## Выполнено в первом шаге

- Создан Cargo workspace `holdem-solver`.
- Зафиксирована архитектура MTT ChipEV-first.
- Добавлен модуль `holdem-cards`:
  - кодировка карт 0..51;
  - rank/suit helpers;
  - deck masks;
  - duplicate-card validation;
  - parser и roundtrip tests.
- Добавлен модуль `holdem-ranges`:
  - exact combos;
  - AA/AKs/AKo generation;
  - диапазоны AA-99 и AKs-ATs;
  - 169-style class id;
  - weighted combos;
  - dead-card filtering;
  - deduplication range classes.
- Добавлен модуль `holdem-evaluator`:
  - единый evaluator для 5, 6 и 7 карт;
  - категории до straight flush;
  - wheel;
  - duplicate-card validation;
  - evaluator tests.
- Добавлен модуль `holdem-equity`:
  - exact profile equity для flop/turn/river;
  - active-player mask;
  - folded cards как dead cards;
  - точный tie split;
  - tests runout count и нормализации equity.
- Добавлен модуль `holdem-domain`:
  - Chips = i64;
  - PlayerState;
  - GameState;
  - Street/Action/TerminalState;
  - pot invariant;
  - базовые state validation tests.
- Добавлен первый слой betting state machine:
  - configure betting context;
  - legal basic actions и configurable bet/raise targets;
  - apply fold/check/call/bet/raise/all-in;
  - pending responders;
  - min-raise validation;
  - pot/stack/commitment updates;
  - fold terminal;
  - regression tests для bet, fold terminal и minimum raise.
- Добавлены table/position/setup-модули:
  - 2–8 seats;
  - button, SB и BB;
  - preflop/postflop order;
  - 8-max positions;
  - uniform ante и big-blind ante;
  - корректное разделение ante и betting contribution;
  - построение initial preflop state.
- Добавлен `holdem-tree`:
  - generic single-round tree builder;
  - street-aware full tree builder;
  - explicit public-card chance nodes;
  - normalized chance probabilities;
  - node ids и parent links;
  - action history reconstruction;
  - decision/chance/round-complete/terminal leaves;
  - max nodes и max depth guards;
  - regression tests для HU preflop root и full flop/turn/river path.
- Добавлен side-pot layer:
  - main pot и side pots по commitment levels;
  - folded players не являются eligible;
  - dead money добавляется в main pot;
  - deterministic odd-chip distribution;
  - tests для multi-stack и folded-player scenarios.
- Добавлен переход между улицами:
  - preflop -> flop -> turn -> river;
  - board-card validation;
  - reset street commitments;
  - новый betting order;
  - остановка на fold terminal при одном active player.
- Улучшены all-in rules:
  - отдельное хранение `raise_allowed`;
  - short all-in не возвращает raise rights уже действовавшим игрокам;
  - all-in как call/raise deduplicated в legal actions;
  - regression test для short all-in reopening.
- Добавлен `holdem-settlement`:
  - exact ChipEV showdown settlement;
  - equity по каждому eligible side pot;
  - folded cards остаются dead;
  - expected payout и net EV по игрокам;
  - multi-pot regression tests.
- Добавлен blocker-aware public board sampler:
  - known hole cards и board исключаются из deck;
  - reproducible seeded sampling;
  - uniform sample probabilities;
  - tests на blockers и reproducibility.
- Добавлен street-specific action abstraction:
  - отдельные настройки preflop/flop/turn/river;
  - absolute bet/raise targets;
  - pot-fraction bets;
  - raise multipliers;
  - integration с tree builder.
- Добавлен tree validator:
  - parent/child consistency;
  - decision/chance/terminal invariants;
  - chance probability normalization;
  - action/chance edge validation.
- Добавлен `holdem-solver-core`:
  - generic two-player extensive-form node interface;
  - CFR+ regret matching;
  - average strategy store;
  - visits/regret metrics;
  - matching-pennies convergence test;
  - external-sampling `MccfrSolver` с deterministic seed;
  - sampled chance/opponent traversal;
  - MCCFR matching-pennies и Kuhn chance-node regressions.
- Добавлен checkpoint/resume слой:
  - versioned `SolverCheckpoint`;
  - deterministic static-game fingerprint;
  - tree/action-abstraction/config fingerprints;
  - JSON serialization через `serde`/`serde_json`;
  - restore validation по algorithm, graph/config fingerprint, infoset и action count;
  - RNG state для external-sampling MCCFR;
  - resume regression tests для CFR+ и MCCFR.
- Добавлен finite-deal Hold'em batch runner:
  - запуск CFR+ или external-sampling MCCFR по compiled private-card game;
  - range -> deals -> compile -> solve vertical slice;
  - average utility и batch metadata;
  - resume compiled/range-conditioned batch;
  - explicit config fingerprint validation.
- Добавлен exact strategy/action-EV report:
  - average action frequencies;
  - counterfactual action EV under average profile;
  - EV loss относительно best action;
  - exact private hand/public node metadata;
  - aggregation shared information sets across opponent deals.
- Добавлен range/class aggregation и result export:
  - blocker-conditioned marginal combo weights;
  - class buckets по `(player, public_node, HandClassId)`;
  - weighted action frequencies и EV;
  - structured action labels;
  - `RangeStrategyReport::to_json()`;
  - versioned JSON schema;
  - complete 13x13 projection через `RangeStrategyReport::to_matrix()`;
  - long-format CSV через `RangeStrategyReport::to_csv()`;
  - по 169 cells для каждого `(player, public_node)`, включая отсутствующие классы.
- Добавлен realistic integration/acceptance pipeline в `crates/solver/tests/realistic_pipeline.rs`:
  - heads-up 100 BB table;
  - full preflop/flop/turn/river tree;
  - street-specific raise/bet abstraction;
  - 9 blocker-conditioned exact private deals;
  - CFR+ solve, strategy report, matrix, JSON и CSV;
  - checkpoint/resume сравнивается с one-shot run.
- Добавлен multiway external-sampling MCCFR foundation в `crates/solver/src/multiway.rs`:
  - dynamic utility vectors для 3+ игроков;
  - multiway static game validation с cycle/reachability checks;
  - player-specific information sets;
  - chance/opponent sampling и traverser action enumeration;
  - deterministic RNG/checkpoint/resume;
  - sampling metrics: traverser updates, chance nodes, opponent actions, infoset visits;
  - three-player checkpoint/resume regression.
- Добавлен multiway Hold'em compiler/payoff в `crates/solver/src/multiway_holdem.rs`:
  - exact weighted 3-8 player private-deal expansion с `max_deals` guard;
  - deterministic rejection sampler conditioned on pairwise blockers;
  - blocker-aware pairwise hand filtering;
  - dynamic ChipEV fold/showdown utility vector;
  - private information sets по `(public node, actor, own Combo)`;
  - public-board conflict filtering и per-deal renormalization;
  - multiway solve/resume APIs;
  - realistic three-way external-sampling integration test.
- Добавлен sampled multiway batch solver в `crates/solver/src/multiway_batch.rs`:
  - новый private profile на каждый traverser update;
  - shared regret/strategy store по `(player, public_node, own Combo)`;
  - отдельные deterministic RNG для private-deal и tree sampling;
  - tree/range/dead-card/config fingerprints;
  - batch checkpoint/resume с private sampler state;
  - sampled utility evaluation и sampling metrics;
  - per-player и aggregate sampling counters;
  - convergence diagnostics: cumulative positive regret, visit-weighted strategy drift и per-player breakdown;
  - online variance/standard-error estimates для sampled average-profile utility;
  - paired per-player best-response probes с strategy value, best-response value, improvement и uncertainty;
  - regression на shared information sets и one-shot/resume equivalence;
  - deterministic `run_parallel()` с worker threads, immutable strategy snapshot и ordered reduction;
  - worker-count invariance и parallel checkpoint/resume regression.
- Добавлены public-tree arena, optional compiled-profile cache и multiway batch result schema:
  - immutable public tree reused across private profiles;
  - blocker-filtered chance outcomes evaluated on demand;
  - terminal ChipEV utility evaluated per sampled profile без копирования solver graph;
  - optional bounded FIFO cache для повторно встречающихся profiles;
  - cache hit/miss/eviction metrics и cache capacity в checkpoint;
  - `MultiwayBatchStrategyReport`;
  - structured JSON и long-format CSV export;
  - exact private cards, hand class id, action frequencies и positive regret;
  - JSON-поля для utility estimate и convergence diagnostics;
  - отдельные JSON APIs для utility estimate, convergence и best-response report.
- Добавлен persistent multiway job layer:
  - `MultiwayBatchJobStore` и JSON manifest;
  - deterministic worker/reduction configuration;
  - atomic checkpoint writes;
  - checkpoint rotation по retention policy;
  - resume из latest durable checkpoint с повторной context validation.
- Добавлен high-level `MultiwayHoldemSpotConfig`:
  - проверка preflop action history против фактического actor;
  - continuation tree для round/full режимов;
  - exact hero combo selection и `AJo` class expansion;
  - aggregate hero strategy report с coverage/missing-combo diagnostics.
- Добавлен versioned JSON spot-job schema:
  - `MultiwayHoldemSpotJob` и explicit action/table/range/tree DTOs;
  - string ranges с optional class weights;
  - exact hero combo или class (`AJo` -> 12 combos);
  - JSON example `docs/examples/multiway_spot_8max_ajo.json`.
- Добавлен native CLI `crates/solver-cli` / binary `holdem-solver`:
  - `validate --job JOB.json` без solver iterations;
  - `solve --job JOB.json --output RESULT.json`;
  - `--job-dir`, `--iterations`, `--utility-samples`, `--job-id`;
  - resume через persistent `MultiwayBatchJobStore`;
  - `--json` success/error envelope для automation;
  - unique temporary output paths перед rename.
- Добавлен native HTTP boundary `crates/solver-server` / binary `holdem-solver-server`:
  - `GET /healthz`;
  - `POST /v1/spot/validate`;
  - synchronous `POST /v1/spot/solve`;
  - queued `POST /v1/jobs`;
  - `GET /v1/jobs/{job_id}` для manifest/status/progress;
  - `GET /v1/jobs/{job_id}/result` для completed JSON result;
  - `POST /v1/jobs/{job_id}/cancel` с cooperative cancellation;
  - persistent data dir с job input, checkpoints и result;
  - body-size, tree-size, job-id path-safety и max-active-jobs guards;
  - стандартный machine-readable JSON error envelope.
- Job lifecycle расширен:
  - `MultiwayBatchJobStatus::Cancelled`;
  - `run_to_target_with_control(...)` проверяет cancellation между checkpoint batches;
  - cancel не прерывает reduction thread посередине batch;
  - regression test cancellation -> resume -> completed.
- Checked-in CLI smoke fixture уменьшен до 2114 nodes:
  - full-tree schema/lifecycle проверяется без многомиллионного дерева;
  - `validate` подтверждён;
  - solve с 2 iterations и resume до 4 iterations подтверждены.
- Добавлена первая browser UI в `frontend/index.html`:
  - readable dark workspace без текстур и мелкого основного текста;
  - отдельные шаги для game setup, history, ranges, tree и execution;
  - validate/queue solve/status polling/cancel/result rendering;
  - analytical result view с 13x13 hero matrix, action cards, table context и compact decision strip;
  - import/export versioned JSON job прямо из UI;
  - multiple weighted public-card outcomes и explicit exact-enumeration control;
  - UI обслуживается native server через `GET /` без внешних CDN ресурсов;
  - visual language намеренно flat/dense/data-led: цвет зарезервирован под actions, без glow/градиентов/декоративных shadows.
- Добавлены пользовательские документы:
  - `docs/EXECUTION_ARCHITECTURE_RU.md` — простое объяснение native/server/browser deployment model;
  - `docs/GETTING_STARTED_RU.md` — Windows setup, создание реального spot, pilot solve, resume, интерпретация и troubleshooting.
- Добавлен GTO-подобный слой UI и запуск в один клик:
  - action-цвета приведены к convention GTO Wizard: Raise красный, Call зелёный, Fold синий, Check жёлтый (отдельный цвет, не grey), Push фиолетовый; легенда matrix обновлена;
  - `frontend/index.html` добавляет позиционные ярлыки UTG/UTG+1/LJ/HJ/CO/BTN/SB/BB в seat selects, history rows, range rows, table diagram, decision strip и validate metrics (offset от button, зеркалит `position_for_seat` из `holdem-domain`);
  - copy UI очищен от случайного английско-русского смешения, тексты объяснения matrix/inspector/diagnostics переписаны коротко и по-русски;
  - `start-solver.bat` (Windows): одна сборка при первом запуске, старт server, ожидание healthz, автопроткрытие браузера, `rebuild` флаг; `stop-solver.bat`; `scripts/start-solver.sh` для Linux;
  - `docs/GETTING_STARTED_RU.md` получает раздел «0. Быстрый старт» с пошаговым «как увидеть матрицу».
- **Result schema v2 — индекс дерева (этап A1 плана `PLAN_RU.md`).** `MultiwayHoldemSpotResult.tree_index`: по записи на compiled-узел (id/parent/street/board/pot/current_bet/dead_money/actor/players/actions[{action,child}]/chance[{cards,child}]/terminal), карты в кодировке `rank*4+suit`, действия — тот же `ActionExport`-формат, что и в strategy report; `MULTIWAY_HOLDEM_SPOT_RESULT_SCHEMA_VERSION=2` отделён от job-схемы, отсутствие `tree_index` = старый результат. UI: панель `Узел дерева` (селектор decision-узлов, чипы-переходы в child, `К hero`), matrix/coverage/inspector переписаны на node-context без hero-хардкода. Проверено: тест `spot_result_exposes_consistent_tree_index`; E2E через server (POST /v1/jobs, 200 iter, result: 2114 узлов, 920 decision / 270 chance / 924 terminal, все child достижимы из parent; 333/334 infosets вне hero-узла доступны браузеру).
- Добавлено поле `Max private attempts` в execution-панели UI (пишется в `execution.max_private_attempts`, default 100000; при импорте читается обратно); demo-фикстура `multiway_spot_8max_ajo.json` переведена на 100000 (на узких 8-max ranges при utility samples 32+ бюджет 10000 исчерпывался и solve падал — эмпирически подтверждено);
- Добавлен первый Hold'em adapter:
  - компиляция валидированного public-information `GameTree` в `StaticGame`;
  - сохранение action metadata рядом с solver child ids;
  - защита от unreachable/cyclic trees;
  - явный отказ от `RoundComplete` без terminal payoff;
  - fixed-hand heads-up fold/showdown ChipEV payoff;
  - showdown payoff подключён к exact side-pot/equity settlement.
- Добавлен finite-deal private-card compiler:
  - private-card chance root;
  - exact `Combo` conditioning;
  - shared information sets по `(public node, actor, own Combo)`;
  - opponent hand не попадает в acting player's infoset;
  - public-card outcomes conflicting with private hands are filtered and renormalized;
  - metadata для public node и private hands сохраняется рядом с solver node.
- Добавлен blocker-aware range expansion:
  - exact `WeightedRange` combo pairs;
  - cross-hand conflict filtering;
  - dead-card filtering;
  - duplicate aggregation;
  - probability normalization.
- Добавлены reference games:
  - Kuhn Poker с explicit six-deal chance node;
  - reduced one-bet-per-round Leduc с private/public-card chance nodes;
  - shared private-card information sets;
  - CFR+ full traversal;
  - Kuhn convergence regression к value `-1/18`;
  - Leduc zero-sum/strategy-normalization regression.

## Пока не реализовано

- Standard multi-raise Leduc reference game (сейчас есть reduced one-bet-per-round variant).
- Durable multi-process queue/scheduler и recovery of in-flight workers (сейчас queue in-process, checkpoints durable).
- Authentication, quotas и multi-tenant isolation.
- Production-grade design system/visual regression для UI.
- ICM.

## Следующий этап

Развивать execution layer без смешивания его с solver core:

1. durable worker scheduler, restart recovery и resource quotas;
2. richer server-side progress/diagnostics stream;
3. UI integration tests, saved presets и visual regression coverage;
4. standard Leduc/multi-raise semantics и отдельный ICM utility layer.

## Проверка

В рабочей среде установлен локальный Rust 1.75 toolchain. Выполнены:

```bash
cargo +1.75.0 fmt --all -- --check
cargo +1.75.0 test --workspace
cargo +1.75.0 test -p holdem-solver-core --test realistic_pipeline -- --nocapture
cargo +1.75.0 test -p holdem-solver-core --test multiway_realistic -- --nocapture
```

Результат: все 88 unit/integration/doc test cases проходят успешно, форматирование корректно. `cargo +1.75.0 check --workspace` также проходит после добавления CLI/server.

CLI smoke/resume:

```bash
cargo +1.75.0 run -p holdem-solver-cli -- validate --job docs/examples/multiway_spot_8max_ajo.json
cargo +1.75.0 run -p holdem-solver-cli -- validate --job docs/examples/multiway_spot_8max_ajo.json --json
cargo +1.75.0 run -p holdem-solver-cli -- solve --job docs/examples/multiway_spot_8max_ajo.json --output /tmp/holdem-cli-result.json --job-dir /tmp/holdem-cli-job --iterations 2 --utility-samples 1 --json
cargo +1.75.0 run -p holdem-solver-cli -- solve --job docs/examples/multiway_spot_8max_ajo.json --output /tmp/holdem-cli-result.json --job-dir /tmp/holdem-cli-job --iterations 4 --utility-samples 1 --json
```

Последовательность фактически завершилась с `completed_iterations=2`, затем `completed_iterations=4`; result имеет format `multiway_holdem_spot_result`. Native server отдельно проверен через `/`, `/healthz`, `/v1/spot/validate`, `/v1/spot/solve` и `/v1/jobs/smoke` на порту `8091`; встроенная UI отдаётся с `GET /` и не использует внешние CDN. Queued lifecycle также проверен: `POST /v1/jobs` -> `Queued` -> `Running` -> `Completed`, а cancellation дал `Cancelled` с durable checkpoint.

Проверка из Windows CMD (из корня репозитория; в CMD используется `^` для переноса строки):

```cmd
cargo +1.75.0 fmt --all -- --check
cargo +1.75.0 check --workspace
cargo +1.75.0 test --workspace
cargo +1.75.0 run -p holdem-solver-cli -- validate --job docs\examples\multiway_spot_8max_ajo.json --json
cargo +1.75.0 run -p holdem-solver-cli -- solve --job docs\examples\multiway_spot_8max_ajo.json --output %TEMP%\holdem-result.json --job-dir %TEMP%\holdem-job --iterations 2 --utility-samples 1 --json
cargo +1.75.0 run -p holdem-solver-cli -- solve --job docs\examples\multiway_spot_8max_ajo.json --output %TEMP%\holdem-result.json --job-dir %TEMP%\holdem-job --iterations 4 --utility-samples 1 --json
```

Server из CMD во втором окне:

```cmd
cargo +1.75.0 run -p holdem-solver-server -- --bind 0.0.0.0:8080 --data-dir %TEMP%\holdem-server-data --max-active-jobs 2
curl.exe http://127.0.0.1:8080/healthz
curl.exe -X POST -H "Content-Type: application/json" --data-binary @docs\examples\multiway_spot_8max_ajo.json http://127.0.0.1:8080/v1/spot/validate
```

В выводе текущего sandbox shell остаются безвредные предупреждения от старой строки в `/home/user/.profile`, которая ссылается на прежний путь Cargo; на код проекта это не влияет.
