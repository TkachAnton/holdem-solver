# Практический запуск solver для конкретных спотов

Этот документ описывает не smoke-test, а рабочий сценарий для реальных continuation spots.

## 1. Что нужно установить на Windows

Нужны:

- Windows 10/11;
- Rust toolchain 1.75+;
- доступный в CMD `cargo`;
- собранный репозиторий `holdem-solver`.

Проверка toolchain:

```cmd
rustc --version
cargo --version
cargo +1.75.0 --version
```

Если `cargo +1.75.0` не найден, установите Rust через официальный `rustup-init.exe`, затем откройте новое CMD окно.

## 2. Сборка native solver

Из корня репозитория:

```cmd
cargo +1.75.0 fmt --all -- --check
cargo +1.75.0 check --workspace
cargo +1.75.0 test --workspace
cargo +1.75.0 build --release -p holdem-solver-cli -p holdem-solver-server
```

После release build основные binaries находятся здесь:

```text
target\release\holdem-solver.exe
target\release\holdem-solver-server.exe
```

## 3. Рекомендуемый локальный режим

Для обычной работы проще всего запускать native server только на loopback interface:

```cmd
mkdir %USERPROFILE%\holdem-solver-data

target\release\holdem-solver-server.exe ^
  --bind 127.0.0.1:8080 ^
  --data-dir %USERPROFILE%\holdem-solver-data ^
  --max-active-jobs 1
```

Затем открыть UI:

```cmd
start http://127.0.0.1:8080/
```

`127.0.0.1` означает, что API доступен только на этом компьютере. Для первого локального использования это безопаснее, чем bind на `0.0.0.0`.

## 4. Как создать реальный spot в UI

### 4.1. Game setup

Заполните:

- количество игроков;
- button seat;
- small blind и big blind;
- stacks каждого игрока;
- ante и ante mode;
- hero seat;
- hero class или exact combo;
- known blockers/dead cards, если они уже известны из spot.

Примеры hero input:

```text
AJo
KQs
JJ
As Jd
```

`AJo` означает запрос всех 12 offsuit комбинаций этого класса. Exact combo вроде `As Jd` означает одну конкретную комбинацию.

### 4.2. Action history

Добавьте действия строго в хронологическом порядке.

History должна описывать уже случившийся betting sequence. Последний action должен оставить actor на hero seat.

Например:

```text
Seat 3 fold
Seat 4 fold
Seat 5 raise to 500
Seat 6 fold
Seat 7 fold
Seat 0 call
```

Solver проверяет actor state самостоятельно. Если history заканчивается на другом игроке, job будет отклонена.

### 4.3. Continuation ranges

Нужно указать отдельный range для каждого seat.

Пример:

```text
Seat 0: AA-TT, AKs-ATs, KQs-KTs, QJs
Seat 1: AJo
Seat 2: AA-TT, AKs-ATs, KQs-KTs, QJs
...
```

Важно: ranges не строятся автоматически из history. Они должны быть ranges уже после указанной истории.

Для настоящего анализа нужно использовать ranges, которым вы доверяете. Если range после raise/3-bet составлен неправильно, solver честно посчитает уже неправильную модель.

### 4.4. Tree abstraction

Начинайте с небольшой action abstraction:

- один или два preflop raise targets;
- один bet fraction на postflop street;
- один raise multiplier;
- разумный `max_nodes`;
- без лишнего all-in branch, если он не нужен.

Например:

```text
Preflop raise targets: 1600
Flop bet fractions: 0.5
Flop raise multipliers: 2
Turn bet fractions: 0.5
River bet fractions: 0.5
```

Чем больше bet/raise branches, тем быстрее растёт tree size. Нельзя считать большое дерево автоматически более качественным: abstraction должна соответствовать вопросу и доступному бюджету.

### 4.5. Public runout

В UI outcomes задаются строками:

```text
2s 7d 9c | 1.0
```

Можно указать несколько weighted outcomes:

```text
2s 7d 9c | 0.4
Ah Kd 4c | 0.3
Jc 8c 2h | 0.3
```

Для turn:

```text
5h | 1.0
```

Для river:

```text
3h | 1.0
```

Это позволяет сначала считать небольшой representative public runout policy. Exact enumeration всех легальных public runouts — отдельный тяжёлый режим. Перед его включением нужно поставить строгий `max_outcomes_per_node` и убедиться, что tree укладывается в память.

### 4.6. Execution

Рекомендуемый процесс:

1. сначала `1000–2000` iterations как pilot;
2. проверить, что job запускается, private profiles легальны, hero combos покрываются;
3. затем увеличить target до `5000`, `10000` или выше;
4. использовать тот же job directory и тот же job id для resume.

`target_iterations` — общий target, не количество дополнительных iterations.

Например:

```text
первый запуск:  target = 2 000
resume:         target = 10 000
```

При resume должны совпадать:

- tree;
- ranges;
- dead cards;
- solver config;
- worker/reduction settings.

Изменение самого target разрешено. Изменение модели spot должно создать новый job id.

## 5. Validate перед solve

Сначала нажмите `Проверить spot`.

Проверяются:

- schema version;
- table configuration;
- history actor sequence;
- hero combo coverage;
- tree construction;
- tree node count;
- tree fingerprint.

Если validate не проходит, solve запускать не нужно.

## 6. Solve и resume

Нажмите `Запустить solve`.

UI отправит asynchronous job и покажет:

```text
Queued
Running
Completed
```

Во время работы доступны:

- completed iterations;
- target iterations;
- последний checkpoint;
- cancel job.

После завершения UI показывает:

- 13x13 hero matrix как главный объект анализа;
- compact history / decision strip;
- aggregate action frequencies с цветовой legend;
- click по matrix cell → exact combo inspector;
- visits и action distribution для каждого наблюдавшегося combo;
- known blockers/dead cards и честную coverage note;
- convergence diagnostics: utility estimate, standard error, positive regret и strategy L1 drift;
- action EV только если он присутствует в result JSON — positive regret не подменяется EV;
- tree nodes и sampling metadata.

Для повторяющихся spot используйте `Presets`: встроенные presets помогают быстро поменять hero class, а `Сохранить preset` сохраняет полную конфигурацию в local browser storage. Это не замена JSON export: для воспроизводимого запуска сохраняйте также JSON job.

## 7. Где сохраняются данные

Если server запущен с:

```text
--data-dir %USERPROFILE%\holdem-solver-data
```

то job хранится примерно так:

```text
%USERPROFILE%\holdem-solver-data\jobs\my-job\job.json
%USERPROFILE%\holdem-solver-data\jobs\my-job\manifest.json
%USERPROFILE%\holdem-solver-data\jobs\my-job\checkpoints\
%USERPROFILE%\holdem-solver-data\results\my-job.json
```

`manifest.json` показывает статус и последний durable checkpoint. Эти файлы не нужно удалять, если планируется resume.

UI также умеет:

- `Импорт JSON` — загрузить сохранённый spot;
- `Экспорт JSON` — сохранить текущую конфигурацию для повторяемого запуска.

## 8. CLI-режим для воспроизводимости

Тот же spot можно запускать без браузера:

```cmd
target\release\holdem-solver.exe validate ^
  --job my-spot.json ^
  --json

target\release\holdem-solver.exe solve ^
  --job my-spot.json ^
  --output my-result.json ^
  --job-dir %USERPROFILE%\holdem-solver-data\jobs\my-spot ^
  --iterations 2000 ^
  --utility-samples 64 ^
  --job-id my-spot ^
  --json
```

Resume:

```cmd
target\release\holdem-solver.exe solve ^
  --job my-spot.json ^
  --output my-result.json ^
  --job-dir %USERPROFILE%\holdem-solver-data\jobs\my-spot ^
  --iterations 10000 ^
  --utility-samples 128 ^
  --job-id my-spot ^
  --json
```

## 9. Как интерпретировать ответ

Action frequency — это стратегия внутри конкретной модели:

```text
strategy = f(history, ranges, tree abstraction, public-card policy, iterations)
```

Поэтому результат нельзя читать как универсальный совет без проверки входных assumptions.

Перед использованием результата проверьте:

- действительно ли history введена без ошибки;
- действительно ли ranges соответствуют continuation state;
- достаточно ли observed hero combos;
- не слишком ли маленький tree;
- не слишком ли мало iterations;
- не слишком ли узкая public-card policy;
- не выросла ли sampling uncertainty.

Отрицательный или неожиданный EV сам по себе не доказывает одну конкретную причину. Сначала нужно проверить pot, investments, ranges, blockers, eligibility и utility normalization.

## 10. Типовые ошибки

### `history ends at actor ... not hero`

Последний action оставляет ход не на hero seat. Исправьте history или hero seat.

### `tree node limit exceeded`

Уменьшите bet/raise branches, max nodes, exact runout policy или начните с representative outcomes.

### `could not sample a legal multiway private deal`

Ranges слишком узкие, конфликтуют друг с другом или с board/dead cards. Расширьте ranges, увеличьте `max_private_attempts` или проверьте blockers.

### `no requested hero combo was visited`

Iterations или private sampling coverage пока недостаточны. Увеличьте pilot target и проверьте, что hero combo присутствует в hero range.

### `private profile has no legal public outcomes`

Указанный board/runout конфликтует со sampled private profile. Используйте корректные public outcomes или более общую chance policy.

## Текущие границы первой версии

Сейчас основной scope:

- MTT NLHE;
- 3–8 игроков;
- ChipEV;
- native/local execution;
- continuation ranges задаются пользователем;
- ICM пока отдельный будущий utility layer.

Для первого реального spot рекомендуется сохранить JSON через UI, сначала выполнить небольшой pilot solve, проверить coverage и только после этого увеличивать iterations или расширять tree.
