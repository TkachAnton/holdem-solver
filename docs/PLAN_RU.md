# План: продвинутый солвер (v1 roadmap)

Дата: 2026-09-11. Основан на: `docs/ARCHITECTURE_RU.md` (этапы 0–8), `docs/STATUS_RU.md`
(«следующий этап»), эталонный UX со скриншота GTO Wizard (`docs/reference/gtow_reference.png`).

Принципиальная рамка: продукт = **native solver engine + native job server + browser UI**
(решение зафиксировано в EXECUTION_ARCHITECTURE_RU). Тяжёлые расчёты никогда не едут в браузер;
UI показывает заранее посчитанное и по запросу новые споты.

## 1. Что уже есть (проверенные факты)

- полный multiway sampled MCCFR: 3–8 игроков, checkpoint/resume, parallel workers,
  fingerprints, детерминированные seeds;
- exact ChipEV: evaluator, profile equity, side pots, settlement; blocker-aware ranges/combos;
- versioned JSON spot-jobs + CLI (validate/solve/resume) + HTTP server (queued jobs, cancel)
  + browser UI с 13x13 матрицей (цвет = доминирующее действие, число = частота),
  combo inspector, blockers, convergence diagnostics;
- `cargo test --workspace`: 88 passed, fmt clean (проверено 2026-09-11).
- `RangeStrategyReport` с exact action-EV существует, но только для heads-up finite-deal пути.

## 2. Ключевые разрывы до эталона

R1. Нет **preflop solution library**: верхняя лента позиций UTG/UTG+1/LJ/HJ/CO/BTN/SB/BB,
    где каждый узел (open 2.1, 3-bet 3.5, jam) — это заранее решённая матрица.
    Ux-сердце GTO Wizard — навигация по решённому дереву, а не ручной solve.
R2. В multiway-результате **нет action-EV и node EV** — матрица не может показать
    режим «стратегия + EV» и числа EV в клетках.
R3. Результат не содержит **индекса дерева** (node id → actor/street/board/pot/actions → child),
    поэтому UI не может переключать узлы без нового solve.
R4. Нет частотного range-редактора (в эталоне дробные combos 223.53, счётчик «N/100»):
    наш ввод ranges = классы/Exact combos, веса классов есть в JSON, но UI их не показывает
    как дробные комбинации.
R5. Нет **стандартных диапазонов-пресетов** (позиция × стек × ante) как точки входа;
    пользователь сейчас обязан задать ranges сам (честно, но далеко от UX эталона).
R6. Нет стрэддла/уровней блайндов в spot-config (в архитектуре этапа 0 значилось).

## 3. Этапы

### A1. Индекс дерева в результате (низкий риск, разблокирует всё)
- `MultiwayHoldemSpotResult` дополняется полем `tree_index`: массив узлов
  `{id, parent, actor, street, board, pot, actions:[{kind,to,child}], terminal?}`;
  schema_version 1→2, back-compat: отсутствие поля = старый результат.
- UI: выпадающий выбор узла (как «Ответы: Высокие карты» в эталоне) + клик по cell-действиям
  перематывает к child-узлу; матрица/экшн-карточки/стол пересчитываются из уже имеющихся
  infosets (player, public_node) — нового solve не требуется.
- Accept: demo-спотbrowse по всем узлам без повторного solve; regression test схемы.

### A2. Action-EV и node EV для multiway
- В solve-report добавить для каждого (player, public_node, class) точные EV действий
  при точном профиле оппонентов (машинерия `RangeStrategyReport::exact action EV`
  переносится на multiway batch; для больших деревьев — опция `utility_samples`-оценка
  с standard error, помечать `ev_kind: exact|sampled`).
- Aggregate EV ноды = взвешенное по частотам диапазонов.
- UI: переключатель режимов матрицы `стратегия | EV | стратегия + EV` (в клетке — номер EV),
  раскраска по знаку EV как доминантный слой.
- Accept: на demo-споте EV совпадают с ручным пересчётом settlement на river-терминалях;
  EV loss относительно best action присутствует; тесты сходимости не деградируют.

### A3. Preflop solution library (product-ядро)
- 3.1 push/fold-библиотека: стеки 8/10/12/15/20bb, 3–9 игроков, единственные линии
  fold/call/jam → дерево маленькое, solve+проверка умещаются в минуты; результат —
  статичные JSON-пресеты в `docs/library/` (версионируются, fingerprint стола в имени).
- 3.2 open+3bet/4bet-библиотека: 25/35/50/75/100bb avg, ограниченные sizing-наборы
  (open 2.0–2.5, 3bet 2.5–3.5x, jam при коротких) — MCCFR до стабилизации
  (avg regret + L1-drift below порога), экспорт по всем узлам;
- UI: верхняя лента позиций (как эталон): карточка позиции = список действий, клик — узел;
  «MTT avg 100bb • ChipEV» в шапке = активный профиль библиотеки, кнопка «изменить» — спот-форма.
- Accept: без нажатия Solve пользователь видит preflop-матрицу любой позиции и любого
  responded-узла из профиля; ответы «почему так» — вкладка EV (после A2).
- Честное ограничение в UI: библиотека = ChipEV на модельных ranges, не «универсальная правда».

### A4. Range-редактор и частотные combos
- ввод вида `AKs:2, A5o:1` (частоты внутри класса) и `range/100` счётчик как в эталоне;
  marginal combo count показывает дробные combos (223.53);
- стандартные диапазоны (пресеты библиотека) как кнопка «подставить», редактируемые.

### A5. Конфиг-полнота
- straddle (опционально), уровни блайндов/ antes как именованные пресеты;
  сохранение/импорт профилей сессий; job-пресеты «MTT 100bb 8max pushfold» и т.п.

### A6. Производительность (параллельно с A3.2)
- профиль solvepreflop-библиотеки: существующие rayon `run_parallel()` + profile cache;
  целевые числа: pushfold-профиль 20bb 6-max ≤ 2 мин на 4 ядрах; 100bb full — часы,
  поэтому библиотека считается офлайн-скриптом и коммитится результатом, не в рантайме юзера.

### A7. Позже (вне v1)
- ICM utility layer (отдельно, как в архитектуре), exploitability/BR-метрики в UI
  (BR-probes уже в движке), multi-raise Leduc, auth/quotas при удалённом сервере.

## 4. Порядок работ

Спринт 1: A1 + A2 (малые, разблокируют UX и эталонные режимы матрицы).
Спринт 2: A3.1 (pushfold-библиотека) + верхняя лента в UI.
Спринт 3: A3.2 + A4. Далее A5/A6, потом A7.

Правило приёмки каждого этапа (как вёл предыдущий агент и обязательно):
fmt → `cargo test --workspace` → realistic-пайплайны → CLI smoke → ручная проверка endpoints;
результат в STATUS_RU; коммит в git.

## 5. Риски

- MCCFR на полных preflop-деревьях 100bb требует больших итераций: EV-цифры будут
  «мягкими»; mitigation — пороги сходимости в UI (уже есть) и режим exact для малых деревьев.
- Схлопывание 169-классов strategy vs exact-combo EV: не путать — отчёт по классам,
  расчёт по комбо (как зафиксировано в архитектуре).
- Совместимость результатов: каждое расширение схемы = schema_version bump + дефолты.
