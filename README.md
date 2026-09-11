# Holdem Solver

Первый этап проекта продвинутого NLHE 8-max MTT ChipEV solver.

## Текущий статус

Реализован начальный foundation slice:

- кодирование карт 0–51;
- deck masks и проверка duplicate cards;
- парсинг preflop range;
- exact combo generation для пар/суited/offsuited рук;
- weighted combo representation;
- единый evaluator 5–7 карт;
- exact profile equity для flop/turn/river;
- базовая модель ChipEV game state;
- invariant checks для pot и игроков.

Уже добавлены базовые street-aware tree/chance nodes, solver-core CFR+ и external-sampling MCCFR toy engines, Kuhn и reduced one-bet-per-round Leduc reference games, finite-deal private-card compiler для HU Hold'em, JSON checkpoints с resume, tree/action-abstraction fingerprints, range-conditioned batch runner, exact strategy/action-EV report, blocker-aware range/class aggregation, complete 13x13 range-matrix projection, versioned JSON/CSV export, multiway external-sampling MCCFR, sampled multiway batch solver и 3-8 player Hold'em compiler/payoff foundation. Первый adapter `holdem-tree -> StaticGame` сохраняет action metadata и использует fold/showdown ChipEV payoff для фиксированных hands. Поверх этого foundation добавлены versioned multiway spot JSON jobs, persistent native CLI с validate/solve/resume, machine-readable CLI envelopes и минимальный native HTTP server boundary для validate/solve/status. Frontend и ICM остаются отдельными слоями.

## Структура

```text
crates/cards          карты и deck masks
crates/ranges         ranges, combos, blockers
crates/evaluator      единый evaluator 5–7 карт
crates/equity         exact profile equity
crates/holdem-domain  игровые состояния, позиции, betting и pots
crates/tree           street-aware tree, chance nodes и action abstraction
crates/settlement     exact ChipEV showdown/side-pot settlement
crates/solver         CFR+/MCCFR, Hold'em tree adapter и ChipEV payoff
crates/solver-cli     native validate/solve/resume CLI
crates/solver-server  native HTTP execution boundary
frontend/index.html    browser UI for the native server
```

## Запуск тестов

В среде с установленным Rust 1.75+:

```bash
cargo test --workspace
cargo fmt --all -- --check
```

В рабочей среде установлен локальный Rust 1.75 toolchain для проверки проекта. На текущем foundation slice выполнены:

```text
cargo +1.75.0 fmt --all -- --check
cargo +1.75.0 test --workspace
```

Для отдельной realistic acceptance-проверки полного finite-deal pipeline:

```bash
cargo +1.75.0 test -p holdem-solver-core --test realistic_pipeline -- --nocapture
cargo +1.75.0 test -p holdem-solver-core --test multiway_realistic -- --nocapture
```

Все проверки проходят успешно. Текущий workspace содержит 88 проходящих unit/integration/doc test cases.

## Native JSON execution

Checked-in smoke job:

```text
docs/examples/multiway_spot_8max_ajo.json
```

Это намеренно небольшой full-tree fixture для проверки JSON/CLI lifecycle: continuation ranges уже заданы после истории, а public runout и postflop abstraction минимальны. Он не является production-range recommendation и не заменяет полноценную postflop abstraction.

CLI:

```bash
cargo +1.75.0 run -p holdem-solver-cli -- validate --job docs/examples/multiway_spot_8max_ajo.json
cargo +1.75.0 run -p holdem-solver-cli -- validate --job docs/examples/multiway_spot_8max_ajo.json --json
cargo +1.75.0 run -p holdem-solver-cli -- solve --job docs/examples/multiway_spot_8max_ajo.json --output /tmp/spot-result.json --job-dir /tmp/spot-job --iterations 2 --utility-samples 1 --json
cargo +1.75.0 run -p holdem-solver-cli -- solve --job docs/examples/multiway_spot_8max_ajo.json --output /tmp/spot-result.json --job-dir /tmp/spot-job --iterations 4 --utility-samples 1 --json
```

`target_iterations` — общий target, а не число дополнительных итераций: увеличение `--iterations` при resume разрешено; tree/range/dead-card/config fingerprints и остальные execution settings должны совпадать. `--json` выдаёт один envelope вида `{\"ok\":true,\"command\":...,\"data\":...}` либо `{\"ok\":false,\"error\":{\"code\":...,\"message\":...}}`.

Native server boundary:

```bash
cargo +1.75.0 run -p holdem-solver-server -- --bind 0.0.0.0:8080 --data-dir ./server-data --max-active-jobs 2
```

Endpoints:

- `GET /` — встроенный browser UI `frontend/index.html`;
- `GET /healthz`;
- `POST /v1/spot/validate` — body is a `MultiwayHoldemSpotJob`;
- `POST /v1/spot/solve` — synchronous body `{\"job\": ..., \"job_id\": \"...\", \"target_iterations\": N, \"utility_samples\": N}`;
- `POST /v1/jobs` — queued background solve с immediate `202 Accepted`;
- `GET /v1/jobs/{job_id}` — persistent manifest/status/progress;
- `GET /v1/jobs/{job_id}/result` — completed JSON result;
- `POST /v1/jobs/{job_id}/cancel` — cooperative cancellation.

Server writes job input, checkpoints and result below `--data-dir`, validates job-id path safety, imposes body/tree limits and returns the same machine-readable success/error envelope. Queued workers checkpoint between batches; authentication, quotas and frontend are deliberately separate layers. Простое объяснение deployment-модели находится в `docs/EXECUTION_ARCHITECTURE_RU.md`. Практическая инструкция для первого реального spot — в `docs/GETTING_STARTED_RU.md`. Первый release планируется как native solver engine + native job server + browser UI, а не как тяжёлый solver внутри браузера.

## Range result export

После получения `RangeStrategyReport` доступны:

```rust
let matrix = report.to_matrix()?;
let json = report.to_json()?;
let csv = report.to_csv()?;
```

`to_matrix()` создаёт отдельную полную 13x13 сетку для каждого `(player, public_node)`:

- порядок строк и столбцов: `A, K, Q, ..., 2`;
- диагональ: пары (`AA`);
- верхний треугольник: suited (`AKs`);
- нижний треугольник: offsuit (`AKo`);
- отсутствующие в конечном finite-deal report классы сохраняются как пустые cells, поэтому каждая сетка содержит 169 ячеек.

JSON имеет `schema_version`, `format`, `algorithm`, `iterations`, `average_utility`, exact `classes` и `matrices`. CSV — long-format: одна строка на cell/action, с пустой action-строкой для отсутствующего класса. `class_id`, `combo_count` и `marginal_weight` остаются blocker-aware данными, а action labels экспортируются структурированно (`kind`, `to`).

Для multiway finite-range batching используются `MultiwayHoldemBatchSolver` и отдельный `MultiwayHoldemBatchCheckpoint`. Solver получает `Vec<WeightedRange>`, сэмплирует новый легальный private profile для каждого traverser update и сохраняет shared strategy store по `(player, public_node, own Combo)`. В checkpoint сохраняются оба RNG state, tree/range/dead-card fingerprints и sampling metrics. По умолчанию batch solver использует immutable public-tree arena и не компилирует отдельный solver graph для каждого private profile. Для сравнений и fallback доступен `new_with_profile_cache_capacity(...)` с bounded compiled-profile cache. `MultiwayBatchStrategyReport` экспортирует exact private cards, action frequencies и positive regret в JSON/CSV; JSON также содержит convergence diagnostics и utility variance. Для production-мониторинга доступны `convergence_diagnostics()`, `evaluate_average_utility_estimate()` и paired `best_response_probe()`: последний даёт per-player strategy value, best-response value, improvement и standard error; это approximate probe, а не математическое доказательство exploitability. Для arena mode добавлен `run_parallel(iterations, worker_count, batch_size)`: worker count не меняет deterministic reduction result, а `batch_size` задаёт границу stale-strategy reduction batch. Для durable execution доступны `MultiwayBatchJobStore`, `MultiwayBatchJobConfig` и `resume_solver(...)`: manifest хранит context fingerprints/status, checkpoints и CLI/server result writes используют уникальные temporary paths перед rename и ротируются по retention policy. Для preflop continuation spots добавлен `MultiwayHoldemSpotConfig`: он валидирует action history против реального actor state, строит continuation tree и агрегирует exact hero combos, включая все 12 комбинаций класса `AJo`. Для пользовательского ввода добавлен versioned JSON schema `MultiwayHoldemSpotJob`; минимальный 8-max пример находится в `docs/examples/multiway_spot_8max_ajo.json`. Ranges в JSON задаются уже для continuation state после указанной истории.

## Принципы foundation

1. Exact combos используются для equity и blockers.
2. Один representative на класс не используется.
3. Деньги хранятся в целых `Chips`, а не в `f32`.
4. Equity cache в следующих этапах обязан учитывать board, exact hole cards и active-player mask.
5. Стратегия всегда привязывается к decision node.
