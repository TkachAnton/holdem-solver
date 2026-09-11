# Как будет работать будущий солвер

## Короткий ответ

Тяжёлая часть солвера планируется как **native Rust application/service**, а не как вычисление внутри браузера.

Браузерный интерфейс уже начат в `frontend/index.html`: в нём пользователь задаёт стол, стеки, историю действий, ranges, hero hand и action abstraction, нажимает Solve и смотрит прогресс/результат. Сам CFR/MCCFR, private-card sampling, checkpoints и ChipEV расчёты выполняются native-процессом.

То есть итоговая схема выглядит так:

```text
Browser UI (vanilla JS/HTML)
        |
        | JSON over HTTP
        v
holdem-solver-server (native Rust)
        |
        v
MultiwayBatchJobStore + native solver workers
        |
        +-- checkpoints/
        +-- manifest.json
        +-- result.json
```

WASM остаётся полезным для лёгких интерактивных вещей, например для отображения/валидации части формы, но не является основным runtime для production MTT NLHE 8-max solve.

## Варианты запуска

### 1. Локальная разработка и автоматизация: CLI

Самый простой и надёжный путь — запускать native binary из terminal:

```text
holdem-solver.exe validate --job spot.json --json
holdem-solver.exe solve --job spot.json --output result.json --job-dir job --iterations 10000 --json
```

Это удобно для batch jobs, CI, экспериментов и повторяемых regression checks. Все важные данные остаются на локальном диске: job input, manifest, checkpoints и result.

### 2. Локальное браузерное приложение

Для обычного пользователя Windows наиболее удобным будет локальное приложение из двух процессов:

1. native `holdem-solver-server.exe` запускается на том же компьютере;
2. браузер открывает frontend;
3. frontend отправляет JSON на `localhost`/loopback server;
4. native server ставит job в очередь;
5. браузер периодически получает status/progress и затем result.

В этой модели браузер не тянет многомиллионное дерево и не расходует память на CFR. Он только показывает форму и результат.

Первая UI уже обслуживается native server через `GET /`. Позже native server можно упаковать вместе с frontend в desktop launcher. Это даст ощущение обычного приложения, хотя внутри всё равно будет native solver service.

### 3. Удалённый solver service

Для production или shared workstation native server может работать на Linux/Windows машине с большим количеством RAM/CPU. Пользователь открывает браузер на своём компьютере, а JSON job уходит по сети на solver host.

Тогда нужны отдельные production layers, которых пока нет:

- authentication;
- authorization и tenant isolation;
- quotas по CPU/RAM/числу jobs;
- TLS/reverse proxy;
- persistent database/object storage;
- worker scheduler;
- observability и audit log.

Текущий server boundary намеренно не делает вид, что эти layers уже реализованы.

## Что происходит после нажатия Solve

Упрощённый жизненный цикл:

1. **Parse JSON.** Проверяются schema version, table/stacks/blinds/antes, history, ranges, hero hand и tree abstraction.
2. **Validate state.** History проигрывается через betting state machine. Последний actor должен совпадать с hero player.
3. **Build public tree.** Создаётся continuation tree с action abstraction и public-card chance policy.
4. **Create/resume job.** Для job создаётся постоянная директория с manifest и checkpoint. Если job уже существует, проверяются tree/range/dead-card/config fingerprints.
5. **Native solve.** Batch solver сэмплирует легальные private profiles и выполняет multiway external-sampling MCCFR/CFR-style updates.
6. **Checkpoint.** После заданного числа iterations стратегия и RNG state сохраняются на диск.
7. **Progress.** Клиент видит `Queued`, `Running`, `Completed`, `Failed` или `Cancelled`, а также `completed_iterations` и последний checkpoint.
8. **Result.** Создаётся versioned JSON result с aggregate hero action frequencies, exact observed/missing combos, positive regret и solver diagnostics.

Ranges автоматически из history не выводятся. В continuation spot они передаются уже для состояния после указанной истории.

## Текущий execution API

Synchronous endpoint для простых smoke/automation сценариев:

```text
POST /v1/spot/solve
```

Queued endpoint для долгих jobs:

```text
POST /v1/jobs
GET  /v1/jobs/{job_id}
GET  /v1/jobs/{job_id}/result
POST /v1/jobs/{job_id}/cancel
```

Queued solve возвращает управление сразу. Worker продолжает работать в native thread, а клиент опрашивает status endpoint. Сейчас это bounded in-process scheduler: одновременно разрешено не больше `--max-active-jobs`, а при превышении сервер возвращает `429`. Checkpoints durable, но очередь ещё не является отдельным multi-process service. Cancel является cooperative: solver завершает текущий checkpoint batch и после этого сохраняет `Cancelled`; thread не прерывается посередине reduction.

## Что будет видеть пользователь

В текущей первой и будущей расширенной web UI это будет примерно так:

- карточка стола и stacks;
- визуальная история preflop действий;
- выбор continuation ranges;
- exact hero combo или class вроде `AJo`;
- настройки bet/raise abstraction;
- кнопка Validate;
- кнопка Solve;
- progress: iterations, checkpoint, sampling metrics;
- hero action matrix/frequencies;
- предупреждения о missing combos, blocker coverage, insufficient samples и convergence uncertainty.

Смысл UI — не скрывать неопределённость. Частота действия после небольшого числа sampled iterations не должна выглядеть как математически окончательная стратегия.

## Рекомендованный практический сценарий для Windows

Для проверки первой UI запускать так.

Окно 1 — native server:

```cmd
cargo +1.75.0 run -p holdem-solver-server -- --bind 127.0.0.1:8080 --data-dir %TEMP%\holdem-server-data --max-active-jobs 2
```

После запуска открыть в браузере:

```text
http://127.0.0.1:8080/
```

Окно 2 — проверка:

```cmd
curl.exe http://127.0.0.1:8080/healthz
curl.exe -X POST -H "Content-Type: application/json" --data-binary @docs\examples\multiway_spot_8max_ajo.json http://127.0.0.1:8080/v1/spot/validate
```

Для batch/CI:

```cmd
cargo +1.75.0 run -p holdem-solver-cli -- validate --job docs\examples\multiway_spot_8max_ajo.json --json
cargo +1.75.0 run -p holdem-solver-cli -- solve --job docs\examples\multiway_spot_8max_ajo.json --output %TEMP%\holdem-result.json --job-dir %TEMP%\holdem-job --iterations 1000 --utility-samples 64 --json
```

## Итоговое архитектурное решение

Первый production release разумно делать не как «солвер в браузере», а как:

```text
native solver engine + native job server + browser UI
```

Это оставляет тяжёлые вычисления в Rust, позволяет нормально использовать CPU/RAM/threads/checkpoints и при этом даёт пользователю удобный браузерный интерфейс. Отдельный desktop wrapper можно добавить позже, не меняя solver core и JSON/server contract.
