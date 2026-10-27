# Headless Ronsole, этап 2 — протокол, `dump`, сценарии L1–L20 — план

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ronsole --headless` принимает команды протокола RRiter (stdin или `--script`), отдаёт `dump`/`dump row`/PNG, и на этом режиме зелёные Rust-сценарии L1–L17, L20 и теста пробуждения; есть драйвер `scripts/ronsole_headless.py` и замер `scripts/measure_rss.py`.

**Architecture:** этап 1 уже дал безоконный runtime (`RuntimeBackend::Headless`, `OffscreenContext`), общий шаг цикла `App::loop_step` и записывающие буфер обмена и открытие ссылок. Этап 2 добавляет: отчёт шага (`StepReport`) и принудительный шаг в шов; разбор протокола и синтез клавиш (`src/headless/protocol.rs`, `keys.rs`); сессию протокола (`session.rs`) поверх методов ввода App; `dump` (`dump.rs`); `bench`; тестовую обвязку и сценарии; драйвер и замер на Python.

**Tech Stack:** Rust (edition проекта), glow/EGL (есть), новые зависимости `serde_json` и `png` (спека §3.2, §6), Python 3 для скриптов.

**Spec:** `/home/reyan/projects/ronsole/docs/superpowers/specs/2026-10-23-ronsole-headless-design.md` (этап 2 — §12; читать §3.3, §4, §5, §6, §7, §9, §10, §11). Эталоны байтов для сценариев: `docs/superpowers/specs/2026-10-21-terminal-parity-design.md` §5.x и `docs/superpowers/specs/2026-10-21-terminal-protocol-ref.md`.

## Global Constraints

* Linux only. Обычный запуск в окне не меняет поведения (§11.5): Wayland-цикл зовёт тот же `loop_step`, отличие только в новом параметре `None` и в отчёте шага, который он игнорирует.
* `runtime.rs` (3754 строки) и `app.rs` (3855) получают только шов и маршрутизацию; логика headless — в `src/headless/` и `src/app/headless_run.rs`.
* Грамматика и ответы протокола — как у RRiter: UTF-8, одна команда на строку, пустые строки и строки с `#` в начале пропускаются; ответ — одна строка `ok`, `ok <payload>` или `err <reason>`; причина ≤ 64 символов; переводы строк в payload — пробел; неизвестная команда — `err unknown command '<name>'`; после `err` следующая команда исполняется. Образец — RRiter `/home/reyan/projects/rriter/src/headless/protocol.rs` (`parse_line` :136, `Response` :81-88, `MAX_ECHO_CHARS` :15, `clip` :285-290).
* Коды выхода: `0` все ответы `ok`; `1` был `err` или нечитаемый ввод; `2` неверные аргументы; `3` EGL или `Renderer` (строка `headless: EGL setup failed at <stage>: <detail>` в stderr).
* Stdout — только ответы протокола; посторонний вывод приложения — stderr.
* Границы: `key --repeat N` N ≤ 1000; `resize` 320x200…8192x8192; `scale` 0.5…4.0; `wait` ≤ 60000; `settle` по умолчанию 500; размер по умолчанию 1280x800, масштаб 1.0; `--hz`/`--budget-ms` взаимоисключающие, по умолчанию 240 Гц; темп кадра `HEADLESS_FRAME_PACE` 8 мс (уже есть, `src/headless/backend.rs:14`).
* Headless не трогает сокет single-instance, системный буфер обмена, `xdg-open`, `$XDG_CONFIG_HOME/ronsole/config` и окружение процесса (Q17, Q27, §3.4).
* Координаты терминала в протоколе (Q24): видимая область — `rows` строк, начиная со строки `top = max(0, total − floor((viewport_h + scroll_offset) / char_h))`, `viewport_h = body.h − top_pad − bottom_pad` (исправлено 25.10 по финальному ревью, спека §5.1); строки от её верха, колонки от 0; концы отрезков ссылок исключающие, у выделения `last` включающий.
* Тесты: `make test TEST_FILTER=<filter>` (TEST_EGL_VENDOR=mesa по умолчанию, `Makefile:25`); `TEST_THREADS` не менять; всё, что тест пишет вне процесса, уникально на процесс (`std::process::id()` в путях). Асинхронный результат ждать `wait_until`, фиксированный `wait` — только для отрицательных проверок и настоящих задержек.
* Без `unwrap()`/`expect()` в продакшен-путях; без `rustfmt`/`cargo fmt` на файлах (дерево не fmt-clean) — стиль соседних строк руками.
* Найден баг вне своей задачи — не чинить: `#[ignore = "bug: <суть>"]`-тест, если дёшево, причина `file:line` и воспроизведение — в отчёт.

## Review Focus

1. **Мусорные аргументы команд** (`resize 0x0`, `scale 10`, `wait 70000`, неизвестный токен `key`, `dump row 999` вне видимой области, не-UTF-8 строка, `mouse_cell` вне видимой области) — ответ `err <причина>`, сессия продолжается, код выхода 1. Тесты — Task 2 (разбор), Task 3 (не-UTF-8, продолжение после `err`), Task 4 (`dump row`/`mouse_cell` вне области).
2. **Приложение выходит посреди скрипта** (`exit` в шелле последней вкладки, `key ctrl+shift+w`) — ответ на текущую команду, следующие строки не читаются, временный профиль удалён, код по прошлым ответам. Тест — Task 3.
3. **Альтернативный экран и прокрутка не на целую строку** — `viewport.scrollback = 0` на alt screen, `cursor`/`text` согласованы; `mouse_cell` по обрезанной нижней строке — `err`. Тесты — Task 4.
4. **Широкие символы** (CJK) — в `dump row` флаги `wide`/`wide_spacer`, `mouse_cell` по широкой клетке попадает в неё. Тест — Task 4.
5. **`--shell` на несуществующий путь** — без фолбэка на `$SHELL`/bash: сообщение об ошибке запуска в сетке, `dump.text` его показывает (§10). Тест — Task 4 (нужен `dump`).

## Общие интерфейсы

| Интерфейс | Где | Задача-владелец | Потребители |
|---|---|---|---|
| `LoopStep::Continue(StepReport)`, `StepReport`, `LoopFlow`, параметр `max_wait_ms` у `loop_step` | `src/app/direct_wayland.rs` | 1 | 3, 4, 5 |
| `App::forced_step`, `App::start_headless`, аксессоры App для headless | `src/app/headless_run.rs` | 1 | 3, 4, 5 |
| Счётчики `frame_requests`/`frames`, `cursor_kind`, `WakeHandle::pending` | `src/headless/backend.rs`, `src/runtime.rs` | 1 | 4 |
| Приёмник открытия ссылок (`Receiver<String>`) из `load_headless` | `src/app/automation.rs` | 1 | 3, 4 |
| `shell_override` | `src/app/*`, `src/terminal_process.rs` | 1 | 3 |
| `Command`, `Response`, `parse_line`, `KeyCombo`, `BenchAction` | `src/headless/protocol.rs` | 2 | 3, 4, 5 |
| `key_spec`, `key_input`, `modifier_key_input` | `src/headless/keys.rs` | 2 | 3, 5 |
| `HeadlessSession`, `SessionOptions`, `run_protocol` | `src/headless/session.rs` | 3 | 4, 5, 6–11 |
| `session_for_test`, `run_script` | `src/headless/tests.rs` | 3 | 4, 8–11 |
| `dump_json`, `dump_row_json`, `cell_center`; `dump`, `wait_until`, `native_wake`, `cell_rect`, `pixel_rgb` в обвязке | `src/headless/dump.rs`, `tests.rs` | 4 | 6–11 |
| Драйвер `scripts/ronsole_headless.py` | `scripts/` | 6 | 7, Task 8 RRiter |

Зависимости: 1 и 2 независимы; 3 после 1 и 2; 4 после 3; 5 после 4; 6 после 5 (драйвер зовёт `bench`); 7 после 6; 8–11 после 4 (друг от друга независимы, у каждой свой файл).

---

### Task 1: Шов — отчёт шага, принудительный шаг, счётчики, приёмник ссылок, шелл

**Files:**
- Modify: `src/app/direct_wayland.rs` (`LoopStep`, `loop_step` :157-251, `direct_wayland_loop`)
- Modify: `src/app/headless_run.rs` (разделить `run_headless_inner`, новые методы App)
- Modify: `src/app.rs` (только видимость: методы ввода `on_*`, `on_frame`, `request_frame`, `prepare_exit` — `pub(crate)` или `pub(super)`, сколько нужно `headless_run.rs`)
- Modify: `src/app/automation.rs` (`load_headless` и `load_headless_with`, :1581-1600 — приёмник выбрасывается в обеих; тест этапа 1 зовёт `load_headless_with`, `headless_run.rs:121`)
- Modify: `src/headless/backend.rs`, `src/runtime.rs` (счётчики, курсор, сброс запроса кадра, `WakeHandle::pending`)
- Modify: `src/headless/mod.rs` (вызов `load_headless` в ветке `--pgo-train`)
- Modify: `src/terminal_process.rs` (:1186, :1212-1243 — шелл по override), путь передачи override от App до спавна — найти исполнителю
- Test: тестовые блоки в `src/app/headless_run.rs`, `src/headless/backend.rs`, `src/terminal_process.rs`

**Interfaces:**
- Produces:
  - `pub(crate) enum LoopStep { Continue(StepReport), Exit(Result<(), String>) }`
  - `pub(crate) struct StepReport { pub(crate) frame: bool, pub(crate) flow: LoopFlow, pub(crate) deadline: Option<Instant>, pub(crate) poll: Option<WaylandPollOutcome> }` — `frame`: шаг нарисовал кадр (`on_frame`); `flow` — из `AppLoopControl` шага; `deadline` — объединённый срок App и ввода, с которым шаг ждал; `poll` — `None`, если шаг ушёл ранним выходом через готовый кадр.
  - `pub(crate) enum LoopFlow { Wait, Poll, WaitUntil }`
  - `pub(super) fn loop_step(&mut self, external_launches: Option<&Receiver<InstanceMessage>>, max_wait_ms: Option<i32>) -> Result<LoopStep, String>` — таймаут ожидания = минимум вычисленного и `max_wait_ms` (`-1` означает «без срока»); Wayland и `--pgo-train` передают `None`.
  - В `src/app/headless_run.rs`, `impl App`:
    - `pub(crate) fn start_headless(&mut self, physical: (u32, u32), scale: f32, launch: TerminalLaunchSpec) -> Result<(), HeadlessRunError>` — первая половина нынешнего `run_headless_inner` (bootstrap + `prepare_bootstrapped_runtime`); `run_headless` зовёт его и затем цикл.
    - `pub(crate) fn headless_step(&mut self, max_wait_ms: Option<i32>) -> Result<LoopStep, String>` — `loop_step(None, max_wait_ms)`.
    - `pub(crate) fn forced_step(&mut self) -> Result<LoopStep, String>` — `on_about_to_wait`; `Exit` → `LoopStep::Exit(take_automation_exit_result())`; иначе сбросить запрос кадра у runtime, нарисовать кадр `on_frame` без темпа и вернуть `Continue(StepReport { frame: true, flow, deadline, poll: None })`, где `flow` — из `AppLoopControl` этого `on_about_to_wait`, `deadline` — объединённый срок App и ввода, как в `loop_step`. Так сессия после команды ввода хранит flow и срок именно этого шага, а не прошлого `headless_step` (`dump.event_loop`, §5.1).
    - `pub(crate) fn headless_exit(&mut self)` — `on_exiting`.
    - аксессоры для `dump` и сессии (имена — на усмотрение, но все здесь, не в `app.rs`): активная вкладка и индекс, все вкладки, `&TerminalInteraction` (layout прошлого кадра), текст `Memory`-буфера (`interaction.clipboard_text()`), `&WindowRuntime`/`&mut WindowRuntime`, конфиг (`terminal_background`, палитра), прокси методов ввода `on_key`, `on_ime_commit`, `on_modifiers`, `on_pointer_motion`, `on_pointer_leave`, `on_pointer_button`, `on_scroll`, `on_focus_changed`, `on_resize_with_logical_size`, `on_scale_changed` — либо эти методы `pub(crate)` (спека §3.1 разрешает).
  - `HeadlessBackend`: `frames: u64` без `#[cfg(test)]`; новое `frame_requests: Cell<u64>` (+1 в каждом `request_frame`); `cursor_kind: Cell<CursorKind>` (пишет `set_cursor_kind` в headless-ветке); `pub(crate) fn clear_frame_request(&self)`. Через `WindowRuntime` — `pub(crate) fn headless_counters(&self) -> Option<HeadlessCounters>` с полями `frames`, `frame_requests`, `redraw_requested`, `cursor_kind`, и `pub(crate) fn clear_frame_request(&self)`.
  - `WakeHandle::pending(&self) -> bool` — `poll(2)` по eventfd с таймаутом 0, без чтения.
  - `pub(crate) fn load_headless(options: Option<AutomationOptions>, profile_dir: &Path, shell: Option<PathBuf>) -> Result<(App, Receiver<String>), String>` — приёмник `OpenSink::Record` больше не выбрасывается (сейчас `automation.rs:1595`); ветка `--pgo-train` держит его живым до конца прогона и не читает. `load_headless_with` (её зовёт `load_headless` и тест этапа 1) тоже отдаёт приёмник и принимает `shell`.
  - Шелл: App хранит `shell_override: Option<PathBuf>`; каждая новая вкладка (и первая, и `ctrl+shift+t`) спавнит именно эту программу, без фолбэка на `$SHELL`/`/bin/bash` (Review Focus 5); `None` — как сейчас (`resolve_terminal_shell`). Ошибка запуска — тот же путь, что ошибка шелла в окне (сообщение в сетке); текст ошибки содержит путь программы (сейчас «failed to spawn terminal shell: …», `terminal_process.rs:342`, и «no supported terminal shell was found», `:1226-1229`, пути не называют), иначе тест Task 4 `missing_shell_reports_in_grid` не найдёт его в сетке.

- [ ] **Step 1: Тесты (красные).** В `headless_run.rs` на headless-runtime (`App::load_headless` с профилем во временном каталоге с `process::id()`, затем `start_headless((640, 400), 1.0, …)`):
  - `step_reports_frame_after_request`: `request_frame`; шаги `headless_step(Some(50))` до первого `frame == true` не дольше 200 мс; счётчик `frames` вырос на 1, `frame_requests` ≥ 1.
  - `idle_step_times_out_with_wait_flow`: после затихания (шаги до `frame == false`) `headless_step(Some(20))` → `poll == Some(WaylandPollOutcome { woke: false, timed_out: true })`, `flow == LoopFlow::Wait`, длительность 15…200 мс.
  - `wake_handle_wakes_step`: `wake_handle().wake()` (`src/wake.rs:32`, его зовёт поток PTY через `notify_main_loop`) → `WakeHandle::pending() == true`; следующий `headless_step(Some(500))` → `poll.unwrap().woke`, длительность < 100 мс.
  - `forced_step_draws_without_pace`: `request_frame`, затем два `forced_step()` подряд → оба `Continue(report)` с `report.frame == true` и `report.poll == None`, `frames` вырос ровно на 2, после них `redraw_requested == false`.
  - `open_request_reaches_receiver`: `open_uri_with("https://example.com", sink)` через `App`-путь открытия ссылки (или прямой вызов с `open_sink` App) → `receiver.recv_timeout(2 s) == Ok("https://example.com")`.
  - В `terminal_process.rs`: `shell_override_is_used_without_fallback` — разрешение шелла по override `/nonexistent/sh` даёт ошибку, а не `/bin/bash`, и текст ошибки содержит `/nonexistent/sh`; override `/bin/sh` даёт `/bin/sh`.
- [ ] **Step 2:** `make test TEST_FILTER=headless_run` и `make test TEST_FILTER=terminal_process` — красные (не компилируются или падают).
- [ ] **Step 3:** реализация по Interfaces. В `loop_step` вычисление причины не делается — только сбор `StepReport`. `direct_wayland_loop` и `run_headless_inner` сопоставляют `LoopStep::Continue(_)`.
- [ ] **Step 4:** те же прогоны — зелёные; `make test TEST_FILTER=headless` — существующие тесты этапа 1 зелёные.
- [ ] **Step 5:** коммит.

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review общий шаг цикла Wayland меняет сигнатуру и ранние выходы; оконный путь не должен изменить поведение, интерфейсы потребляют Task 3–5

### Task 2: Разбор протокола и синтез клавиш

**Files:**
- Create: `src/headless/protocol.rs`, `src/headless/keys.rs`
- Modify: `src/headless/mod.rs` (только `mod protocol; mod keys;`)
- Modify: `src/wayland_input.rs`, `src/input_types.rs` — только видимость нужных функций/типов (`should_emit_xkb_text`, `named_term_key`, `physical_key_from_evdev`, `KeyInput` и поля), без изменения логики
- Modify: `src/input.rs` — только видимость `terminal_key_event` (:1334) для эталонной проверки в тесте `keys.rs`
- Test: тестовые блоки в обоих новых файлах

**Interfaces:**
- Consumes: `KeyInput { state, physical_key, keycode, text, unshifted, named, ime_preedit }`, `KeyState`, `Modifiers` (`src/input_types.rs`); `should_emit_xkb_text` (`src/wayland_input.rs:398-430`), `named_term_key` (:470-488), `physical_key_from_evdev` (:68).
- Produces (`protocol.rs`):
  - `pub(crate) enum Command { Type(String), Key { combo: KeyCombo, repeats: u32 }, Mods(Modifiers), MouseMove { x: f64, y: f64 }, MouseCell { col: usize, row: usize }, Click { button: MouseButtonArg, phase: ClickPhase, alt: bool }, Wheel { dx: f64, dy: f64, unit: WheelUnit }, Resize { w: u32, h: u32 }, Scale(f32), Screenshot(PathBuf), Dump(Option<PathBuf>), DumpRow(usize), Settle { ms: u64 }, Wait { ms: u64 }, Wake { ms: u64 }, Idle { ms: u64 }, Bench { frames: u32, csv: Option<PathBuf>, action: BenchAction }, Info, Quit }`
  - `pub(crate) enum MouseButtonArg { Left, Right, Middle }`, `pub(crate) enum ClickPhase { Both, Down, Up }`, `pub(crate) enum WheelUnit { Lines, Pixels }`, `pub(crate) enum BenchAction { None, Wheel { dx: f64, dy: f64 }, Key(KeyCombo), Type(String) }`
  - `pub(crate) struct KeyCombo { pub(crate) key: KeySpec, pub(crate) mods: Modifiers }`
  - `pub(crate) enum Response { Ok(Option<String>), Err(String) }` с `pub(crate) fn line(&self) -> String` (клип причины до 64 символов, переводы строк payload → пробел)
  - `pub(crate) fn parse_line(line: &[u8]) -> Result<Option<Command>, String>` — `Ok(None)` для пустой строки и `#`-комментария; `Err(reason)` → сессия отвечает `err <reason>`.
  - `dump`: строка ровно из двух токенов `row` и `<uint>` — `DumpRow`; иначе остаток — путь (`Dump(Some)`); пусто — `Dump(None)`. `type`: остаток строки, экранирование `\n`, `\t`, `\\`, прочее буквально. Команды RRiter `open`, `workspace`, `dialog`, `picker_answer`, `record`, `dblclick` — `err unknown command '<name>'`.
- Produces (`keys.rs`):
  - `pub(crate) struct KeySpec { pub(crate) keycode: u32, pub(crate) unshifted: Option<char>, pub(crate) shifted: Option<char> }`
  - `pub(crate) fn key_spec(name: &str) -> Option<KeySpec>` — имена как RRiter `/home/reyan/projects/rriter/src/keymap.rs:49-88` (`enter`, `numpadenter`, `tab`, `escape`/`esc`, `backspace`, `delete`, `space`, стрелки, `home`/`end`, `pageup`/`pagedown`, `insert`, `pause`, `f1`–`f12`, пунктуация, `a`–`z`, `0`–`9`); keycode — evdev (`linux/input-event-codes.h`), символы — US-раскладка.
  - `pub(crate) fn with_layout_char(spec: KeySpec, c: char) -> KeySpec` — `--char C` (Q19): `unshifted = C`, `shifted = C` в верхнем регистре.
  - `pub(crate) fn key_input(spec: &KeySpec, state: KeyState, mods: Modifiers) -> KeyInput` — `physical_key = physical_key_from_evdev(keycode)`; `unshifted = spec.unshifted`; `text` — если `should_emit_xkb_text(state, physical_key, mods, false, state == Repeat, false)`, то `shifted` при Shift, иначе `unshifted`, строкой; иначе `None`; `named = named_term_key(keycode, ks, ks)`, где `ks` — keysym символа `unshifted` (для печатных ASCII 0x20–0x7e keysym равен коду символа), 0 для клавиш без символа; `ime_preedit = false`.
  - `pub(crate) fn modifier_key_input(modifier: Modifiers, state: KeyState) -> KeyInput` — левые клавиши: ctrl 29, shift 42, alt 56, super 125; `named` через `named_term_key`, `text = None`, `unshifted = None`.
  - Порядок модификаторов комбинации — ctrl, shift, alt, super (используется в Task 3).

- [ ] **Step 1: Тесты (красные).** `protocol.rs`: таблица `parse_line` → ожидание: `type a\tb\\n` → `Type("a\tb\\n")` по правилу экранирования (`\t` → таб, `\\` → `\`, далее `n` буквально); `key --repeat 3 ctrl+shift+a`; `key --char с ctrl+c` (keycode 46, unshifted `'с'`); `mods ctrl shift`; `mods` → пустой набор; `mouse_cell 3 4`; `click right down alt`; `wheel 0 -3` → `Lines`; `wheel 0 -40 px` → `Pixels`; `resize 1280x800`; `scale 1.333`; `dump` / `dump /tmp/a.json` / `dump row 5` / `dump row 5 x` (→ путь `row 5 x`); `settle` → 500; `bench 100 csv=/tmp/b.csv key ctrl+c`; `info`, `quit`, `# c`, пустая строка → `None`. Негативные: `key --repeat 1001 a`, `key foo` → `unknown key token 'foo'`, `resize 0x0`, `resize 9000x10`, `scale 10`, `wait 60001`, `dump row -1` (путь, не ошибка — фиксировать это поведение по правилу двух токенов), `open x` → `unknown command 'open'`, не-UTF-8 байты → `Err`. `Response::line` клипает причину до 64 символов и меняет `\n` в payload на пробел.
  - `keys.rs`: `key_input(c, Pressed, CONTROL)` → keycode 46, `text == None`, `unshifted == Some('c')`; `shift+a` → `text == Some("A")`, `unshifted == Some('a')`; `enter` без модификаторов → `text == None` (Wayland-правило), `physical_key == PhysicalKey::Code(KeyCode::Enter)`; `pause` → `PhysicalKey::Unidentified`, keycode 119; `with_layout_char(c, 'с')` + Ctrl → `unshifted == Some('с')`, `text == None`; без Ctrl → `text == Some("с")`, с Shift → `Some("С")`; `modifier_key_input(CONTROL, Pressed)` → keycode 29, `named == Some(TermKey::Modifier(ModKey::LeftCtrl))`. Проверочный эталон: `key_input` для `--char с ctrl+c`, переданный в `terminal_key_event` (`src/input.rs:1334`), даёт то же, что тест `key_event_maps_russian_ctrl_c` (`src/input.rs:1613-1624`; ссылка спеки `:1561-1570` устарела): `physical == 46`, `key == TermKey::Char('с')`, `base == Some('c')`, `mods.ctrl`.
- [ ] **Step 2:** `make test TEST_FILTER=headless::protocol`, `make test TEST_FILTER=headless::keys` — красные.
- [ ] **Step 3:** реализация. Правило текста и keycode модификаторов — из Wayland-функций, без копий.
- [ ] **Step 4:** те же прогоны — зелёные.
- [ ] **Step 5:** коммит.

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review синтез `KeyInput` должен совпасть с Wayland-путём (`runtime.rs:1634-1661`) и с эталоном `key_event_maps_russian_ctrl_c`; разбор — общий контракт с RRiter

### Task 3: Сессия протокола, запуск, команды ввода и цикла, обвязка тестов

**Files:**
- Create: `src/headless/session.rs`, `src/headless/tests.rs` (обвязка, `#[cfg(test)]`), `src/headless/tests_protocol.rs`
- Modify: `src/headless/mod.rs` (`parse_args` §4.1, ветка протокола вместо «protocol mode is not implemented yet» :89, `mod` новых файлов; `Profile` — режимы `ProfileChoice`: сейчас `Drop` всегда удаляет каталог, :102-127, — удаляется только `Temp { keep: false }`, `Temp { keep: true }` и `Dir` остаются; тест этапа 1 `headless_args_without_pgo_train_rejected`, :147-152, ждёт «protocol mode is not implemented yet» — переписать: `--headless` без `--pgo-train` теперь режим протокола, код 2 дают только ошибки разбора из `bad_args_exit_2`), `Cargo.toml` (`png`, `serde_json`), `src/runtime.rs` (два метода GL-части ниже)
- Test: `src/headless/tests_protocol.rs`

**Interfaces:**
- Consumes: Task 1 (`start_headless`, `headless_step`, `forced_step`, `headless_exit`, аксессоры и методы ввода App, `load_headless(…, shell)`, `StepReport`, `LoopFlow`, `headless_counters`), Task 2 (`Command`, `Response`, `parse_line`, `key_input`, `modifier_key_input`, `KeyCombo`), этап 1 (`Profile::create`, `prefer_egl_vendor`, `tune_glibc_allocator`, `OffscreenContext::gl_strings`).
- Produces:
  - `pub(crate) struct SessionOptions { pub(crate) size: (u32, u32), pub(crate) scale: f32, pub(crate) profile: ProfileChoice, pub(crate) shell: Option<PathBuf>, pub(crate) budget: FrameBudget }`
  - `pub(crate) enum ProfileChoice { Temp { keep: bool }, Dir(PathBuf) }`; `pub(crate) struct FrameBudget { pub(crate) budget_ms: f64, pub(crate) hz: f64, pub(crate) source: HzSource }`; `pub(crate) enum HzSource { Arg, Default }`
  - `pub(crate) struct HeadlessSession` (владеет `App`, профилем, приёмником открытия ссылок, удерживаемыми `mods`, позицией указателя, последним `StepReport`) с `pub(crate) fn start(options: SessionOptions) -> Result<Self, StartError>` (`StartError::Egl(String)` → код 3, прочее — как ветка `--pgo-train` в `headless::main` сейчас), `pub(crate) fn execute(&mut self, command: Command) -> Response`, `pub(crate) fn exited(&self) -> bool`, `pub(crate) fn app_mut(&mut self) -> &mut App` (для `dump` в Task 4).
  - `pub(crate) fn run_protocol(options: SessionOptions, input: impl BufRead, output: &mut impl Write) -> u8` — код выхода по Global Constraints; EOF — как `quit`.
  - `WindowRuntime`: `pub(crate) fn resize_offscreen(&mut self, width: u32, height: u32) -> Result<(), String>` (новый pbuffer до App, Q25; при ошибке ничего не меняется) и `pub(crate) fn read_pixels(&self) -> Result<(u32, u32, Vec<u8>), String>` (RGBA8 текущего кадра, строки сверху вниз, при текущем контексте сессии).
  - Обвязка (`tests.rs`): `pub(crate) fn session_for_test(w: u32, h: u32) -> HeadlessSession` (вызывает `prefer_egl_vendor`; профиль во временном каталоге с `process::id()` и счётчиком; шелл `/bin/sh`; в конфиге профиля явный `terminal_background` — `pub(crate) const TEST_BACKGROUND: [u8; 3]`, формат конфига — как его читает `AppConfig::load_at`), `pub(crate) fn run_script(session: &mut HeadlessSession, script: &[u8]) -> Vec<String>`.

Решения по командам (спека §3.3, §4.2):
* Старт: одна вкладка терминала, затем `settle 500`.
* `type` — `on_ime_commit`, один принудительный кадр (`forced_step`).
* `key [--repeat N] <combo>` — последовательность спеки §4.2 дословно: недостающие среди удерживаемых модификаторы по порядку ctrl, shift, alt, super — `modifier_key_input(.., Pressed)` + `on_modifiers(новый набор)`; `Pressed` клавиши, N раз `Repeat`; кадр; `Released`; добавленные модификаторы в обратном порядке — `Released` + `on_modifiers`; кадр. `on_key` вернул `true` — выход.
* `mods` — разница с текущим набором теми же событиями модификаторов, один кадр. Удерживаемые действуют на мышь и складываются с `key` и `click alt`.
* `mouse_move x y` — физические пиксели буфера; в `on_pointer_motion` передать координаты в тех единицах, в которых их передаёт Wayland-путь (найти в `runtime.rs`, обработка pointer motion), один кадр.
* `click` — `on_pointer_button(Pressed/Released, кнопка)`, кадр на каждую фазу; `alt` — Alt добавляется к удерживаемым на время клика событиями модификатора; `true` из `on_pointer_button` — выход.
* `wheel dx dy [lines|px]` — `on_scroll(ScrollDelta::Line|Pixel)` с `dx`, `dy` как есть, без смены знака (как RRiter, `/home/reyan/projects/rriter/src/headless/mod.rs:524-527`), один кадр. У Ronsole `dy > 0` — вверх (`input.rs`, `mouse_wheel`): к старым строкам — `wheel 0 3`. Колесо действует, только когда указатель в `layout.body` (`layout.body.contains`); по умолчанию указатель в (0, 0) (`app.rs:469`), вне `body`, поэтому перед `wheel` нужен `mouse_cell`/`mouse_move` в тело. Одна строка колеса — 4 строки терминала × чувствительность (`input.rs:1117`); прокрутка анимирована (`on_frame`: `scroll_y.update(dt)`), итог ждут `wait_until`, а не читают после команды.
* `resize WxH` — `resize_offscreen` → при ошибке `err`, App не зовётся; иначе `on_resize_with_logical_size` (логический = физический / масштаб) и кадр; `ok <w>x<h>`.
* `scale S` — физический размер прежний, масштаб backend и `on_scale_changed`, кадр.
* `screenshot <out.png>` — если кадр запрошен, сначала принудительный кадр; `read_pixels`; каталоги создаются; PNG крейтом `png`; `ok <abs path> <w>x<h>`.
* `settle [ms]` — шаги `headless_step(Some(2))`, пока подряд идущие простои (кадр не нарисован, `redraw_requested == false`, срок шага `None` или позже конца бюджета) не покроют не меньше 2 × `TERMINAL_OUTPUT_BATCH_IDLE` (`terminal_process.rs:25`, 8 мс) по реальным часам, или до бюджета; `ok frames=<n> settled=<bool>`. Причина: поток разбора будит цикл только после пачки, ждущей новый вывод 8 мс (`terminal_process.rs:1058-1064`, `:1100`), и два шага по 2 мс принимали за тишину вывод, ещё сидящий в пачке.
* `wait <ms>` — шаги `headless_step(Some(min(2, остаток)))` до срока по реальным часам; `ok frames=<n>`.
* `wake <ms>` (Q18) — один шаг `headless_step(Some(ms))`; перед ним запомнить `headless_counters().redraw_requested` (кадр запрошен до шага). Причина — по тому, чем кончилось ожидание шага, по порядку: шаг нарисовал без ожидания (`poll == None`, `frame == true`) — `redraw`; кадр был запрошен до шага и ожидание кончилось по сроку (`poll.timed_out`) — добавочный `headless_step(Some(0))`: нарисовал (срок темпа наступил) — `redraw`, нет — дальше по порядку ниже (`timed_out` одинаков для срока `ms`, `WaitUntil` и `Poll`); `flow == Poll` — `poll`; `poll.woke` — `ui_waker`; срок шага (`deadline`) прошёл — `deadline`; иначе `timeout`. После `poll`, `ui_waker`, `deadline` — тоже добавочный `headless_step(Some(0))` (если его ещё не было): шаг, проснувшийся по eventfd, только запрашивает кадр (`take_frame_wake` → `request_frame`, `direct_wayland.rs:240-246`), а рисует следующий. Причину выбирать по отчёту первого шага (`poll`, `flow`, `deadline`); добавочный шаг заново зовёт `on_about_to_wait` и пересчитывает срок, поэтому влияет только на `frame` и поля ответа. `frame` — нарисовал ли кадр хоть один из шагов; `flow`/`deadline_ms` ответа — последнего. Отличие от «одного шага» спеки (§3.3): добавочный шаг сам делает poll с таймаутом 0 и может забрать eventfd следующего вывода; пробуждение не теряется — оно уходит в `request_frame`, и следующий `wake` даст `redraw`. Причину нельзя выбирать после шагов по готовности кадра: вывод PTY будит eventfd, кадр сразу готов (темп прошёл, `backend.rs:71-80`), и `ui_waker` не появлялся бы никогда (ср. RRiter: причина выбирается до рисования, `rriter/src/headless/mod.rs:757-785`). Ответ `ok cause=<c> frame=<bool> flow=<wait|poll|wait_until> deadline_ms=<n|none>` (`deadline_ms` — до срока на момент ответа, не меньше 0).
* `idle <ms>` — `wake` с остатком времени, пока не истечёт `ms`; `ok frames=<n> redraws=<n> polls=<n> wakes=<n> deadlines=<n> flow=<f> deadline_ms=<n|none>` (`wakes` — число `ui_waker`, `flow`/`deadline_ms` — последнего шага).
* `info` — `ok {"gl_renderer","gl_version","gl_vendor","profile","budget_ms","hz"}`.
* `dump`, `dump row`, `mouse_cell`, `bench` в этой задаче отвечают `err not implemented` (их делают Task 4 и 5).
* Выход App (`headless_step` или `forced_step` вернул `Exit`, `on_key`/`on_pointer_button` вернули `true`) — `headless_exit`, ответ на текущую команду, дальше ввод не читается.
* Последний `StepReport` сессии — от любого шага, `headless_step` или `forced_step`: из него `dump.event_loop.control_flow`/`deadline_ms` (Task 4).
* `parse_args`: флаги §4.1 `--script FILE --size WxH --scale S --profile DIR --keep-profile --shell PATH --hz N | --budget-ms F`; `--hz` вместе с `--budget-ms`, неизвестный флаг, `--pgo-train` вместе с флагом протокола — код 2. Ветка `--pgo-train` этапа 1 не меняется.

- [ ] **Step 1: Тесты (красные)** в `tests_protocol.rs`, каждый через `session_for_test(640, 400)` или `run_protocol` с `Cursor`-входом; маркеры собраны из частей (`'RE''ADY'`):
  - `info_reports_budget`: `info` → JSON, `hz == 240`, `budget_ms` ≈ 4.1667 (±0.001), непустой `gl_renderer`.
  - `type_and_key_reach_shell`: каталог процесса `tmp`; `type cat > <tmp>/out\n`, `key shift+a`, `key --char с a`, `key enter`, `key ctrl+d`; `wait` шагами до появления файла с содержимым `"Aс\n"` (не дольше 5 с) — файл ровно такой.
  - `err_does_not_stop_session`: `run_protocol` на `resize 100x100\ninfo\n` → две строки, первая `err …`, вторая `ok {…`, код 1; на `info\n\xff\xfe\ninfo\n` → `ok`, `err`, `ok`, код 1; на `info\nquit\ninfo\n` → две строки, код 0.
  - `app_exit_ends_script`: `run_protocol` на `type exit\n` + `wait 5000\n` + `info\n` → ровно две строки `ok…`, код 0, временный каталог профиля удалён.
  - `resize_and_screenshot`: `resize 800x600` → `ok 800x600`; `screenshot <tmp>/a/b.png` → `ok <abs> 800x600`, файл декодируется `png` с размером 800x600; `scale 1.333` → `ok`, следующий `screenshot` — по-прежнему 800x600.
  - `idle_session_waits_without_frames`: `type : > <tmp>/r\n`, `wait` шагами до появления файла (`dump` и `wait_until` даёт только Task 4; к появлению файла эхо уже выдано, а приглашение `/bin/sh` печатает только после команды), фиксированный `wait 200` — настоящая задержка под приглашение, затем `settle`, затем `wake 50` → `ok cause=timeout frame=false flow=wait deadline_ms=none`; `idle 300` → `frames=0`; `settle` → `settled=true`.
  - `shell_output_wakes_by_ui_waker`: `type : > <tmp>/r; sleep 2; printf x\n`, `wait` шагами до появления файла `<tmp>/r` (эхо строки ушло из PTY раньше), `settle`, затем `wake 5000` → `cause=ui_waker frame=true`, шаг `wake` занял ≥ 1000 мс (проснулся по выводу `printf`, а не по эху).
  - `bad_args_exit_2`: `parse_args` на `--hz 60 --budget-ms 5`, на `--pgo-train --script x`, на `--bogus` — ошибка, `headless::main` даёт код 2.
- [ ] **Step 2:** `make test TEST_FILTER=headless::tests_protocol` — красные.
- [ ] **Step 3:** реализация по решениям выше.
- [ ] **Step 4:** `make test TEST_FILTER=headless` — зелёные (вместе с этапом 1, Task 1–2).
- [ ] **Step 5:** коммит.

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review модель шагов `settle/wait/wake/idle` и последовательность событий `key`/`mods` — контракт с RRiter и основа всех сценариев Task 4–11

### Task 4: `dump`, `dump row`, `mouse_cell`, обвязка проверок

**Files:**
- Create: `src/headless/dump.rs`, `src/headless/tests_dump.rs`
- Modify: `src/headless/session.rs` (команды `Dump`, `DumpRow`, `MouseCell`; хранение последнего `external_request`), `src/headless/tests.rs` (helper'ы ниже)
- Modify (только доступ, без логики): `src/terminal.rs` (снимок под парой блокировок), `src/renderer.rs` (метрики шрифта терминала), `src/renderer/terminal_ui.rs` (`terminal_cell_render_colors` и геометрия вкладок — `pub(crate)`), `src/app/headless_run.rs` (аксессоры, если Task 1 их не дал)
- Test: `src/headless/tests_dump.rs`

**Interfaces:**
- Consumes: Task 3 (`HeadlessSession`, `app_mut`, `session_for_test`, `run_script`), Task 1 (аксессоры, `headless_counters`, `WakeHandle::pending`, приёмник ссылок, последний `StepReport` в сессии).
- Produces:
  - `pub(crate) fn dump_json(session: &mut HeadlessSession) -> serde_json::Value` — ключи §5.1 спеки дословно: `size`, `scale`, `cursor_icon` (`format!("{:?}", CursorKind)`), `tabs[{index,title,active,rect}]`, `terminal{cols,rows,viewport{top,scrollback,total},text,cursor,title,modes{app_cursor_keys,bracketed_paste,mouse_tracking,mouse_sgr,focus_reporting,kitty_flags,sync_pending},selection,hovered_link,layout{body,cell0,char_w,char_h,baseline,x_height}}`, `clipboard{mode,text}`, `external_request{kind,arg}|null`, `event_loop{control_flow,deadline_ms,wake_pending,redraw_requested,frame_requests,frames}`.
  - `pub(crate) fn dump_row_json(session: &mut HeadlessSession, row: usize) -> Result<serde_json::Value, String>` — §5.2 дословно; вне видимой области — `Err("row out of range")`.
  - `pub(crate) fn cell_center(layout: &TerminalUiLayout, top: usize, total: usize, rows: usize, col: usize, row: usize) -> Option<(i32, i32)>` — `cell0` и клетка по формуле §5.1 из полей `TerminalUiLayout` (`body`, `text_x`, `char_w`, `char_h`, `bottom_pad`, `scroll_offset`, `cols`) (`x0`, `x1`, `y0`, центр `(floor((x0 + x1) / 2), floor(y0 + char_h / 2))`); `None`, если клетка вне видимой области или центр вне `layout.body`. Одна функция для `mouse_cell` и `dump.terminal.layout`, без второй копии формулы.
  - `Terminal`: `pub(crate) fn lock_snapshot(&self) -> (MutexGuard<'_, ()>, MutexGuard<'_, Term>)` — порядок `bridge_screen_lock`, затем `grid` (как поток разбора, `terminal_process.rs:1069-1070`).
  - Обвязка (`tests.rs`): `pub(crate) fn dump(session: &mut HeadlessSession) -> serde_json::Value`, `pub(crate) fn wait_until(session: &mut HeadlessSession, timeout_ms: u64, what: &str, done: impl FnMut(&serde_json::Value) -> bool) -> serde_json::Value` (крутит `wait 50`, по таймауту паника с `what` и `dump`), `pub(crate) fn native_wake(session: &mut HeadlessSession, ms: u64) -> (String, Instant)`, `pub(crate) fn cell_rect(dump: &serde_json::Value, col: usize, row: usize) -> (i32, i32, i32, i32)` (`x0, y0, x1, row_bottom` по формуле §5.1), `pub(crate) struct TestImage { pub(crate) width: u32, pub(crate) height: u32, pub(crate) rgba: Vec<u8> }`, `pub(crate) fn screenshot(session: &mut HeadlessSession) -> TestImage` (команда `screenshot` во временный каталог процесса, декодирование крейтом `png`) и `pub(crate) fn pixel_rgb(image: &TestImage, (x, y): (i32, i32)) -> [u8; 3]`.

Решения:
* Раздел `terminal` и `dump row` — одной парой блокировок `lock_snapshot`. Видимая область (Q24): `total = term.total_rows()`, `top = max(0, total − floor((viewport_h + layout.scroll_offset) / char_h))`, `viewport_h = body.h − top_pad − bottom_pad` (та же высота, что у `rows` и `max_scroll` в `terminal_ui.rs:1006-1066`; исправлено 25.10 по финальному ревью); при `scroll_offset > 0` и `scroll_offset + 0.5 ≥ max_scroll` — `top = 0` (округлённое смещение кадра против дробного `max_scroll`, решение A-1), `top ≤ total − rows`; `text[v] = term.row(top + v)` с срезанными хвостовыми пробелами; `cursor` — `scrollback_len + cur_y − top`, `null` вне `[0, rows)`; `selection` — `normalized_selection_bounds` и перевод строк в видимую область (могут быть отрицательными); `hovered_link` — `LinkTarget.cells` в видимую область, `col_end` исключающий; `layout` — `null` до первого кадра.
* `title` — `write_display_title`. `mouse_tracking` — `"none"|"press"|"button_motion"|"any_motion"`. `sync_pending` — `sync_deadline().is_some()`.
* `dump row`: цвета — `terminal_cell_render_colors(cell, палитра кадра)` → `#rrggbb` (`round(c · 255)`), `bg` `null` для фона по умолчанию; `flags` — `bold, dim, italic, inverse, strike, conceal, wide, wide_spacer` (без `HOST_HIDDEN`); `underline` — имя `UnderlineStyle` в нижнем регистре; `link` — `hyperlink().uri`; пустая ячейка без атрибутов пропускается.
* `external_request`: сессия при каждом `dump` вычерпывает приёмник, хранит последний аргумент, отдаёт `{"kind":"open_url","arg":…}` и очищает (второй `dump` — `null`).
* `mouse_cell c r` — `cell_center` → `None`: `err`; иначе как `mouse_move x y`, ответ `ok <x> <y>`.
* `dump` без пути — `ok <json одной строкой>`; `dump <out.json>` — JSON в файл (каталоги создаются), ответ `ok <abs path>` (спека §4.2).

- [ ] **Step 1: Тесты (красные)** в `tests_dump.rs` (`session_for_test(800, 600)`, маркеры из частей):
  - `dump_text_cursor_and_title`: `printf '\033]2;T''T\007RE''ADY'; sleep 30` (без `sleep` `/bin/sh` печатает приглашение в ту же строку и сдвигает курсор) → `wait_until` маркер в `terminal.text`; `title == "TT"`; `cursor.row` — строка маркера, `cursor.col == 5` (длина маркера).
  - `viewport_scrollback_and_wheel`: `seq 1 <3·rows>`; `viewport.scrollback > 0`, `top == total − rows`; `mouse_cell 0 0` (указатель в `body`), `wheel 0 1` → `wait_until` `top` = прежний − 4 (одна строка колеса — 4 строки терминала, прокрутка анимирована), `text[0]` — соответствующее число `seq`. `dump /tmp/<каталог процесса>/d.json` → `ok <abs path>`, файл — тот же JSON.
  - `alt_screen_has_no_scrollback`: `printf '\033[?1049h'` → `viewport.scrollback == 0`, `top == 0`.
  - `dump_row_colors_and_wide`: `printf '\033[1;38;2;200;10;20mR\033[0m界'` → в `dump row` клетка `R`: `fg == "#c80a14"`, `bg == null`, `flags` содержит `bold`; `界` — `wide`, следующая клетка — `wide_spacer`; `mouse_cell` по колонке `界` даёт `x` внутри `cell_rect` его клетки. `dump row 999` → `err row out of range`.
  - `mouse_cell_matches_cell_rect`: `mouse_cell 2 1` → `ok x y`, `(x, y)` внутри `cell_rect(dump, 2, 1)`; `mouse_cell 9999 0` → `err`.
  - `background_pixel_matches_config`: `printf '\033[48;2;10;200;30m  \033[0m'` → пиксель центра клетки (0, строка) — `(10, 200, 30)` ±2 на канал; пиксель пустой клетки ниже — `TEST_BACKGROUND` ±2.
  - `frame_requests_grow_on_output`: два `dump` вокруг `type x` → `event_loop.frame_requests` вырос; `cursor_icon` — строка; `tabs.len() == 1`, `tabs[0].active`, `tabs[0].rect` не `null`.
  - `missing_shell_reports_in_grid`: сессия с `shell: Some("/nonexistent/sh")` → `wait_until` в `terminal.text` есть строка с `/nonexistent/sh`.
- [ ] **Step 2:** `make test TEST_FILTER=headless::tests_dump` — красные.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** `make test TEST_FILTER=headless` — зелёные.
- [ ] **Step 5:** коммит.

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review ключи и система координат `dump` — общий контракт с RRiter (Task 8 RRiter), формулы §5.1 и порядок блокировок

### Task 5: `bench`

**Files:**
- Create: `src/headless/bench.rs`
- Modify: `src/headless/session.rs` (команда `Bench`), `src/renderer.rs` (счётчики кадра `flush_calls` и `vertices` в `flush` :982-1026, сброс в начале кадра; чтение `pub(crate) fn frame_stats(&self) -> (u32, u32)`)
- Test: тестовый блок `bench.rs`

**Interfaces:**
- Consumes: Task 3 (`HeadlessSession`, `forced_step`, `FrameBudget`), Task 2 (`BenchAction`).
- Produces: `bench <frames> [csv=<path>] [action]` → `ok <json>`.

Решения: образец — RRiter `docs/headless.md` раздел Bench (`/home/reyan/projects/rriter/docs/headless.md:171-215`) и его реализация в `/home/reyan/projects/rriter/src/headless/`. Перед каждым из `frames` принудительных кадров — действие (`none`, `wheel dx dy` в строках, `key <combo>` нажатие+отпускание, `type <text>`). Колонки CSV: `update_ms`, `draw_cpu_ms`, `gpu_ms` (`GL_TIME_ELAPSED` вокруг рисования + `glFinish`; пусто, если запросы времени не поддержаны), `total_ms`, `flush_calls`, `vertices`, `scroll_y` (текущее смещение прокрутки активного терминала после кадра, px, не цель анимации). Колонок `root_*_ms`/`chrome_*_ms` нет: это фазы рендера редактора RRiter, у Ronsole их нет (спека Q29). Действие `wheel` действует, только когда указатель в `layout.body` (Task 3, решение `wheel`). CSV — в `csv=<path>` или `${XDG_RUNTIME_DIR:-/tmp}/ronsole-headless/bench-<unix_ms>.csv`. Summary: `frames`, `budget_ms`, `hz`, `hz_source` (`arg|default`), p50/p95/p99/max для `total_ms`, `gpu_ms` (или `null`), `draw_cpu_ms`, `over_budget`, пять `worst`, `system` (`loadavg_before/after`, `cpus`, `gpu_util_before/after` из `nvidia-smi` или `null`, `process_cpu_ms`), абсолютный `csv`. Счётчики рендера — два целочисленных инкремента в `flush`, без аллокаций.

- [ ] **Step 1: Тест (красный)** `bench_writes_csv_and_summary`: `bench 5 csv=<tmp>/b.csv` → JSON `frames == 5`, `hz_source == "default"`, `total_ms.p50 > 0`; CSV — заголовок + 5 строк, `flush_calls ≥ 1`, `vertices > 0` в каждой; `bench 3 type x` → `frames == 3`; после `seq 1 <3·rows>` и `mouse_cell 0 0` — `bench 2 wheel 0 1` → `ok`, `scroll_y` в CSV меняется между строками.
- [ ] **Step 2:** `make test TEST_FILTER=headless::bench` — красный.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** тот же прогон — зелёный.
- [ ] **Step 5:** коммит.

Агенты: implementer ds-low-agentic · reviewer нет (лист: замер; драйвер Task 6 только передаёт команду `bench` и печатает ответ, от формата summary не зависит)

### Task 6: Драйвер, справка, самопроверка протокола

**Files:**
- Create: `scripts/ronsole_headless.py`, `docs/headless.md`, `scripts/headless_selftest.txt`

**Interfaces:**
- Consumes: Task 3–5 (бинарь с протоколом).
- Produces: драйвер для Task 7 и для Task 8 RRiter: подкоманды `shot`, `bench`, `run <script|->`, `repl`; общие флаги `--size`, `--scale`, `--profile`, `--keep-profile`, `--shell`, `--hz`, `--budget-ms`; бинарь `target/x86_64-unknown-linux-gnu/release/ronsole` (переопределение `RONSOLE_BIN`; нет — подсказка `make fast`, код 2); `err` в ответе — код 1; снимки по умолчанию — `/tmp/ronsole-headless`; `shot` без аргумента-цели — одна вкладка, `settle`, `screenshot`, печатает путь PNG.

Решения: порт `/home/reyan/projects/rriter/scripts/rriter_headless.py` (280 строк) без подкоманд, которым нет места в Ronsole (открытие файла, workspace). `docs/headless.md` — короткая справка: запуск, команды §4.2, ключи `dump` §5 (ссылкой на спеку, без копии таблиц), коды выхода, «перед `wheel` указатель в `body` (`mouse_cell`)», «`settle` с бюджетом не больше 2 × `TERMINAL_OUTPUT_BATCH_IDLE` (16 мс) почти всегда `settled=false`», отличия от RRiter (нет `open`/`workspace`/`dialog`/`picker_answer`/`record`/`dblclick`; вкладки — `key ctrl+shift+t` и клик по `dump.tabs[].rect`). `headless_selftest.txt` — сценарий, где по разу встречаются все команды общего тела RRiter (Q26): `type`, `key`, `key --char`, `mods`, `mouse_move`, `mouse_cell`, `click`, `wheel`, `resize`, `scale`, `screenshot`, `dump`, `dump row`, `settle`, `wait`, `wake`, `idle`, `info`, `quit` — каждая отвечает `ok`.

- [ ] **Step 1:** `make fast` (release-бинарь; один раз).
- [ ] **Step 2:** драйвер, справка, сценарий.
- [ ] **Step 3:** проверка: `env -u WAYLAND_DISPLAY python3 scripts/ronsole_headless.py --shell /bin/sh run scripts/headless_selftest.txt` → код 0, все ответы `ok`; `python3 scripts/ronsole_headless.py shot` → путь PNG существует; `RONSOLE_BIN=/nonexistent python3 scripts/ronsole_headless.py shot` → код 2 с подсказкой `make fast`.
- [ ] **Step 4:** коммит.

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review драйвер — контракт с Task 7 и Task 8 RRiter (подкоманды, флаги, коды выхода, `shot`)

### Task 7: Замер памяти `scripts/measure_rss.py`

**Files:**
- Create: `scripts/measure_rss.py`

**Interfaces:**
- Consumes: Task 6 (драйвер или прямой запуск `ronsole --headless` через stdin), release-бинарь.

Решения (спека §7): `ronsole --headless --shell /bin/sh` без `WAYLAND_DISPLAY`; простой 3 с, затем `VmRSS` из `/proc/<pid>/status` и `Rss`, `Pss`, `Private_Dirty` из `/proc/<pid>/smaps_rollup` процесса Ronsole; затем `resize`, пока `terminal.cols` не станет 120 (ширина по `terminal.layout.char_w` первого `dump`), 10 000 строк по 120 колонок (`type` команды шелла, печатающей их), ожидание последней строки по `dump`, тот же замер; 3 прогона, медиана; вывод — таблица «простой / полный скроллбэк» в kB. Аргументы: путь бинаря (по умолчанию release), `--runs` (3). Старый `measure-rss.sh` в `.superpowers/` не трогать.

- [ ] **Step 1:** скрипт.
- [ ] **Step 2:** проверка: `env -u WAYLAND_DISPLAY python3 scripts/measure_rss.py --runs 3` → код 0, две строки медиан, все значения > 0, полный скроллбэк больше простоя.
- [ ] **Step 3:** коммит.

Агенты: implementer ds-low-agentic · reviewer нет (лист: замер)

### Общее для Task 8–11 (сценарии §9)

Каждая задача — свой файл `src/headless/tests_<группа>.rs` (подключить `mod` в `src/headless/mod.rs` — одна строка; конфликт строк `mod` при вливании решает главная). Правила сценариев — спека §9.1 целиком (шелл `/bin/sh`, `type`, длинный сценарий — файлом во временном каталоге процесса и `sh <файл>`; маркер собран из частей; читатель байтов, отданных приложению, — дословно из §9.1; пиксели — `cell_rect` и `TEST_BACKGROUND`). Перед `wheel` указатель в `body` (`mouse_cell`); к старым строкам — `wheel 0 <+N>`, одна строка колеса — 4 строки терминала, итог прокрутки — `wait_until` (Task 3, решение `wheel`). Ожидания — из спеки §9.2 и эталонов `docs/superpowers/specs/2026-10-21-terminal-parity-design.md` §5.x и `docs/superpowers/specs/2026-10-21-terminal-protocol-ref.md`, не из вывода кода. Сценарий упал из-за дефекта Ronsole, а не теста — тест остаётся с `#[ignore = "bug: <суть>"]`, причина `file:line` и воспроизведение — в отчёт; код Ronsole не чинить. Прогоны — только своего файла: `make test TEST_FILTER=headless::tests_<группа>`.

### Task 8: Сценарии отрисовки — L1, L11, L12, L13, L14, L17

**Files:**
- Create: `src/headless/tests_render.rs`

**Interfaces:**
- Consumes: Task 4 (`session_for_test`, `run_script`, `dump`, `wait_until`, `cell_rect`, `screenshot`, `pixel_rgb`, `TEST_BACKGROUND`).

- [ ] **Step 1:** сценарии по строкам §9.2: L1 (каждый атрибут SGR строки таблицы 5.1 спеки паритета → `dump row` `flags`/`underline`/`underline_color`/`fg`/`bg`; conceal — пиксели клетки цвета фона), L11 (bold шире/темнее regular той же буквы — сумма тёмных пикселей; символ без грани начертания рисуется), L12 (верх подчёркивания не выше `layout.baseline` клетки; цвет подчёркивания — `underline_color` при SGR 58, иначе `fg`; цвет зачёркивания — `fg`), L13 (нет пикселей подчёркивания при `y ≥ row_bottom` при `scale 1.0` и `scale 1.333`, строка ниже пустая), L14 (центр полосы зачёркивания в ±1 px от середины чернил глифа `x` той же строки, по PNG), L17 (нет `/usr/share/fonts/**/NotoColorEmoji.ttf` по пути из `src/renderer.rs:364-366` — тест падает с сообщением, называющим файл; иначе эмодзи в bold и regular дают одинаковые пиксели клетки).
- [ ] **Step 2:** `make test TEST_FILTER=headless::tests_render` — не больше 3 прогонов, затем отчёт с падениями.
- [ ] **Step 3:** коммит.

Агенты: implementer ds-low-agentic · reviewer нет (лист: тесты; ветку проверяет финальное ревью)

### Task 9: Сценарии ввода — L2, L8, L9, L10

**Files:**
- Create: `src/headless/tests_input.rs` (helper читателя §9.1 — здесь же, он нужен только этой задаче)

**Interfaces:**
- Consumes: Task 4 (обвязка), Task 3 (`key --char`, `mods`, `mouse_cell`, `click` фазы).

- [ ] **Step 1:** сценарии по строкам §9.2: L2 (DA2 `CSI > 1 ; <major·10000 + minor·100 + patch из CARGO_PKG_VERSION> ; 0 c`, XTVERSION `DCS > | ronsole(<версия>) ST`, DECRQM, OSC 10/11/12/4 с `?` — настоящие цвета конфига профиля; весь hex-вывод читателя сравнивается целиком), L8 (Kitty: флаги, стеки, release/repeat по таблицам спеки 5.x и справочника; Shift+Enter; `key --char с ctrl+c` → `03`; IME — `type` не-ASCII текстом), L9 (`mods ctrl` над ссылкой → `cursor_icon` — указатель-рука (имя варианта `CursorKind`, который ставит Ronsole при Ctrl над ссылкой), пиксель подчёркивания в `cell_rect`; при `ESC[?1000h` и читателе `mods ctrl` + `click` → `external_request` (`wait_until`), фиксированный `wait`, `mods`, `key x` — весь вывод читателя `78`), L10 (отчёт мыши: `mouse_cell c r` + `click` → SGR с `c+1;r+1`; 1004 (только Ronsole): читатель во вкладке 1, `key ctrl+shift+t` → `ESC[O`, клик по `dump.tabs[0].rect` → `ESC[I`, вывод — в `dump.text` вкладки 1; paste: выделение протягиванием `mouse_cell` + `click left down` → `mouse_cell` → `click left up`, `key ctrl+c`, читатель с `ESC[?2004h`, `key ctrl+v` → `1b5b3230307e … 1b5b3230317e`).
- [ ] **Step 2:** `make test TEST_FILTER=headless::tests_input` — не больше 3 прогонов, затем отчёт с падениями.
- [ ] **Step 3:** коммит.

Агенты: implementer ds-low-agentic · reviewer нет (лист: тесты; ветку проверяет финальное ревью)

### Task 10: Сценарии ссылок — L4, L5, L15, L16, L20

**Files:**
- Create: `src/headless/tests_links.rs`

**Interfaces:**
- Consumes: Task 4 (обвязка, `hovered_link`, `external_request`, `dump row` `link`).

- [ ] **Step 1:** сценарии по строкам §9.2: L4 (OSC 8 ссылка, затем 2·`rows` строк, `wheel` вверх до строки ссылки — `dump row` `link`; затем `resize` — та же проверка), L5 (http, https, file URL: `mods ctrl` + `mouse_cell` → `hovered_link.uri`; `click` → `external_request` `{"kind":"open_url","arg":<URL>}` через `wait_until`), L15 (указатель над ссылкой без Ctrl, `dump`; `mods ctrl` без движения → `frame_requests` вырос, `hovered_link` не `null`), L16 (ссылка на две строки и два отрезка в строке → `hovered_link.ranges` точно), L20 (исполняемый файл и `.desktop` во временном каталоге процесса, Ctrl+клик по `file://`-ссылке на него → `external_request.arg == "file://<канонический каталог, percent-encoded как platform/process.rs:375-397>"`, `wait_until`).
- [ ] **Step 2:** `make test TEST_FILTER=headless::tests_links` — не больше 3 прогонов, затем отчёт с падениями.
- [ ] **Step 3:** коммит.

Агенты: implementer ds-low-agentic · reviewer нет (лист: тесты; ветку проверяет финальное ревью)

### Task 11: Сценарии сессии — L3, L6, L7, пробуждение

**Files:**
- Create: `src/headless/tests_misc.rs`

**Interfaces:**
- Consumes: Task 4 (обвязка, `native_wake`, `event_loop`, `clipboard`), Task 3 (`idle`).

- [ ] **Step 1:** сценарии по строкам §9.2: L3 (`printf '\033[?2026h'; printf 'vis''ible\n'; sleep 1`; отсчёт от первого `dump` с `terminal.modes.sync_pending` (`wait_until`) — в нём маркера нет; маркер виден не позже 150 + 100 мс от этого `dump`; отдельный запуск — `idle 400` после начала буфера → `deadlines ≥ 1`), L6 (OSC 52 запись → `dump.clipboard == {"mode":"memory","text":<текст>}`; до записи `text == null`), L7 (OSC 7 на временный каталог процесса, `key ctrl+shift+t`, `pwd` в новой вкладке → путь в `dump.text`), пробуждение (Q18: шелл печатает строку через `sleep 1`; `idle 2000` → `wakes ≥ 1`, строка в `dump.text` после `idle`).
- [ ] **Step 2:** `make test TEST_FILTER=headless::tests_misc` — не больше 3 прогонов, затем отчёт с падениями.
- [ ] **Step 3:** коммит.

Агенты: implementer ds-low-agentic · reviewer нет (лист: тесты; ветку проверяет финальное ревью)

## Завершение (главная сессия, не задача)

После `complete` всех задач: `env -u WAYLAND_DISPLAY make codex_test` по интеграционной ветке (§11.1); финальное ревью ветки (`ds-high` + `final-reviewer`) по спеке §11.1, §11.3, §11.4 (предусловия RRiter Task 8: команды `mods`, `mouse_cell`, `dump row`, `key --char`, ключи §5.1) и §11.5; PR в Ronsole `main`. §11.4 целиком (общее тело `scripts/term_parity/` RRiter исполняется драйвером Ronsole) проверяет Task 8 плана RRiter: тела ещё нет.

