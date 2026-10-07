# rsloop: ізоляція free-threaded callback RSS

Дата: 7 жовтня 2026. Baseline: `43f624a` (код #95 до version-only release v0.1.58).

## Висновок

У відтворених запусках головна причина додаткового peak RSS — непорожній
ambient Context у free-threaded interpreter, а не відсутність fast `tp_dealloc`.
Після import path benchmark він містить warnings ContextVar (`_warnings_context`
у виміряному runtime). Free-threaded CPython 3.14 за замовчуванням вмикає
context-aware warnings; звичайний build їх вимикає.

`capture_callback_context` замінює лише **порожній implicit Context** на None.
Для непорожнього Context він правильно зберігає окремий snapshot на callback.
Це необхідна семантика: ContextVars, warnings filters і tokens не повинні
змінювати контекст інших callbacks.

Після вирівнювання workload через запуск усередині нового `contextvars.Context()`
FT peak RSS зменшується з **193.28 до 132.00 MiB** (−61.28 MiB, −31.7%).
Медіанний total time: **223.35 → 221.85 ms**; діапазони перекриваються,
тому timing improvement не доведений. Це before/after **контролю контексту в
benchmark**, не виправлення allocations production-застосунку.

Зміна destructor на GIL build: **110.78 → 110.79 MiB**; total time
**177.06 → 194.22 ms**. Fast path впливає на dispatch, але не пояснює RSS gap.
На тому самому FT binary `PYTHON_GIL=0` і `1` дають відповідно
**193.28 / 193.26 MiB** з ambient Context і **132.00 / 132.01 MiB** з порожнім.

## Методика

- Linux x86_64, kernel 6.18.44, glibc 2.39; affinity CPU 0.
- Обидва CPython: 3.14.0, python-build-standalone/uv distributions,
  Clang 21.1.4. Повні `CONFIG_ARGS`, версія, executable, GIL state, allocator,
  warnings flag і ContextVar names збережені в JSON.
- Звичайний і FT builds мають різні ABI/allocator/header; CONFIG_ARGS також
  відрізняються JIT flag. Це не контрольована локальна збірка з одного source
  tree. Runtime GIL перевірено окремо на **тому самому FT binary**.
- Rust extension: однаковий baseline, release, locked dependencies, rustc 1.99.0.
  Додатковий GIL control build використовує лише `standard-handle-dealloc`.
- Timing: 1,000,000 callbacks, 2 warmups + 7 виміряних свіжих subprocesses,
  GC вимкнений; RSS містить interpreter/libraries. RSS delta від baseline перед
  створенням loop. Timing і lifetime/tracemalloc прогоняються окремо.
- Lifetime: 1,025 sampled weakrefs для 1,000,000 handles. Cleanup: 100,000
  callbacks, 1,031 sampled callable payloads, local і external-thread producers.
- Allocation probe: 100,000 callbacks; `tracemalloc` captures Python domains,
  а не Rust queue allocations. Його timing/RSS не входить до timing table.
- Shared host, послідовний порядок cases; це microbenchmark, не статистично
  ізольований CPU test. Сирі runs і діапазони наведені для перевірки.

## Timing і RSS

| Case | Peak RSS, MiB | Peak delta, MiB | Queue, ms | Dispatch, ms | Total, ms | Total range, ms |
|---|---:|---:|---:|---:|---:|---:|
| GIL build, ambient, fast destructor | 110.78 | 85.61 | 94.31 | 82.35 | 177.06 | 175.73–177.95 |
| GIL build, ambient, standard destructor | 110.79 | 85.64 | 98.75 | 91.80 | 194.22 | 184.56–217.20 |
| FT build, GIL off, ambient | 193.28 | 161.14 | 125.17 | 96.55 | 223.35 | 219.25–255.82 |
| FT build, GIL on, ambient | 193.26 | 161.08 | 129.41 | 95.72 | 227.28 | 219.07–239.47 |
| GIL build, controlled empty context | 110.80 | 85.57 | 104.84 | 90.74 | 200.54 | 181.25–208.96 |
| FT build, GIL off, controlled empty context | 132.00 | 99.86 | 110.89 | 105.72 | 221.85 | 207.17–242.61 |
| FT build, GIL on, controlled empty context | 132.01 | 99.83 | 115.54 | 107.21 | 222.32 | 214.53–232.16 |
| GIL build, one ContextVar | 172.04 | 146.90 | 122.92 | 88.79 | 212.34 | 200.71–216.50 |
| FT build, GIL off, one ContextVar | 193.43 | 160.99 | 126.21 | 96.94 | 224.25 | 221.12–230.42 |

## Об'єкти й lifetime

| Build / implicit Context | Allocations на callback | Traced bytes на callback |
|---|---:|---:|
| GIL / empty | 1 Handle | 64 |
| GIL / nonempty | 1 Handle + 1 Context | 128 |
| FT / empty | 1 Handle | 80 |
| FT / nonempty | 1 Handle + 1 Context | 144 |

На спільному scheduling line для 100,000 callbacks: 100,001 blocks / 6,400,064
bytes (GIL empty), 100,001 / 8,000,064 (FT empty), 200,000 / 12,800,000
(GIL nonempty), 200,000 / 14,400,000 (FT nonempty). Малий додатковий block
походить від loop/task bookkeeping. None-marker забирає саме Context snapshot.

FT Handle `tp_basicsize` = 80 проти GIL 64; GC tracking відсутній. Залишковий
RSS gap для empty Context узгоджується з +16 bytes на 1M handles (~15.26 MiB),
більшим interpreter baseline та allocator overhead. Rust ready queues спільні
для цих build paths, але їхня пам'ять не входить до tracemalloc.

Peak уже існує у фазі `queued`, до запуску будь-якого batch callback.
У lifetime probe sample alive зменшується 1,025 → 512 → 256 → 0. Після resume,
return, close і GC sample handles не залишаються. На FT RSS після deallocation
може залишатися високим: живі Python objects і allocator-retained pages — різні
величини. GC не повертає ці pages автоматично OS.

Local і external-thread cleanup probes звільнили всі 1,031 sampled payloads
до resume. Після close + GC traced current memory allocation probes становить
менше 18 KiB, а не мегабайти утриманих callback objects. Ознак backlog deferred
references у цих сценаріях немає. Це спостереження через weakrefs/finalizers і
tracemalloc, **не прямий лічильник внутрішнього PyO3 reference pool**; воно не
виключає проблеми інших workload paths. Peak-before-dispatch окремо виключає
відсутній fast destructor як головне джерело цієї різниці.

## Patch

- `benches/callback_rss.py`: timing/lifetime/allocations/cleanup; явні
  `--context ambient|empty|nonempty`; local/thread producer; JSON metadata,
  phased RSS, weakrefs і payload finalizers.
- Передавання `sys._xoptions` дочірнім interpreter у новому та основному
  `compare_event_loops.py`. Без цього parent `-X context_aware_warnings=0`
  мовчки губиться при запуску child; тепер контроль застосовується фактично.
- Feature `standard-handle-dealloc`: контрольний GIL build без fast destructor.
  Default destructor paths не змінено, FT slot mutation не додано.
- Regression checks: empty context, options propagation, N vs 2N allocations,
  bounded residual traced memory, release external-thread payloads.

Не пропонується unsafe FT `tp_dealloc` replacement: RSS не дає для цього підстав.
Не пропонується вимикати context-aware warnings у production. Для реальних
nonempty contexts наступна оптимізаційна ціль — snapshot allocations; можливе
sharing immutable backing лише з гарантією snapshot isolation і коректною
інвалідацією після ContextVar mutation. Поточне дослідження не реалізує і не
доводить безпечність такого cache. Сам Context спільно використовувати не можна.

## Відтворення

```bash
uv venv --python 3.14.0 target/bench-gil
uv venv --python 3.14.0t target/bench-ft
uv pip install --python target/bench-gil/bin/python maturin pytest pytest-mock
uv pip install --python target/bench-ft/bin/python maturin pytest pytest-mock
VIRTUAL_ENV="$PWD/target/bench-gil" target/bench-gil/bin/maturin develop --release --locked
VIRTUAL_ENV="$PWD/target/bench-ft" CARGO_TARGET_DIR="$PWD/target/ft" target/bench-ft/bin/maturin develop --release --locked
# Зберегти package copy з обома ABI перед перезбиранням GIL control.
mkdir -p target/baseline/python
cp -r python/rsloop target/baseline/python/
PYTHONPATH="$PWD/target/baseline/python" taskset -c 0 target/bench-gil/bin/python benches/callback_rss.py --output target/gil.json
PYTHON_GIL=0 taskset -c 0 target/bench-ft/bin/python benches/callback_rss.py --output target/ft0.json
PYTHON_GIL=1 taskset -c 0 target/bench-ft/bin/python benches/callback_rss.py --output target/ft1.json
PYTHON_GIL=0 taskset -c 0 target/bench-ft/bin/python benches/callback_rss.py --context empty --output target/ft0-empty.json
PYTHON_GIL=0 taskset -c 0 target/bench-ft/bin/python benches/callback_rss.py --context nonempty --mode allocations --count 100000 --repeat 1 --warmups 0 --output target/ft-allocations.json
PYTHON_GIL=0 taskset -c 0 target/bench-ft/bin/python benches/callback_rss.py --mode cleanup --context empty --producer thread --count 100000 --repeat 1 --warmups 0 --output target/ft-cleanup.json
VIRTUAL_ENV="$PWD/target/bench-gil" target/bench-gil/bin/maturin develop --release --locked --features standard-handle-dealloc
taskset -c 0 target/bench-gil/bin/python benches/callback_rss.py --output target/gil-standard.json
```

Обрати доступний CPU через `os.sched_getaffinity(0)`. Для точного історичного
baseline checkout `43f624a`, потім застосувати diagnostic patch. Artifact patch
rebased на поточний master, щоб не скасувати нові upstream зміни.

Validation: 37 tooling tests на GIL fast, FT GIL-off і FT GIL-on; 29 existing
fast-callback tests на FT GIL-off і GIL standard destructor; release compilation
для обох ABI й контрольної feature. Нові файли проходять Ruff. Основний benchmark
має три попередні Ruff violations (EXE001, два BLE001); вони не внесені patch.

## Первинні джерела та межа висновку

- [rsloop context capture, baseline](https://github.com/RustedBytes/rsloop/blob/43f624a/src/context.rs)
- [rsloop PyHandle layout, baseline](https://github.com/RustedBytes/rsloop/blob/43f624a/src/engine/callbacks.rs)
- [rsloop destructor guard, baseline](https://github.com/RustedBytes/rsloop/blob/43f624a/src/bindings/loop_api/fast_callbacks.rs)
- [CPython 3.14 defaults](https://github.com/python/cpython/blob/v3.14.0/Python/initconfig.c)
- [CPython warnings ContextVar](https://github.com/python/cpython/blob/v3.14.0/Lib/_py_warnings.py)
- [CPython GIL/FT object headers](https://github.com/python/cpython/blob/v3.14.0/Include/object.h)

Оригінальний host #96 не доступний. Його ContextVar contents/config не записані,
тому історичні 111.7/193.5 MiB не можна остаточно приписати тій самій причині.
Ця причина підтверджена intervention experiments для відтворення на тому самому
rsloop commit і пояснює RSS pattern майже тієї самої величини.
