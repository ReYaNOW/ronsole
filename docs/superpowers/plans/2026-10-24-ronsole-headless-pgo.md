# Headless Ronsole, этап 1: PGO без окна — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ronsole --headless --pgo-train …` проходит PGO-автоматизацию без Wayland и без окна; `scripts/pgo_pipeline.py` тренирует без `WAYLAND_DISPLAY`; после приёмки PGO-бинарь ставится в `~/bin/system/ronsole`.

**Architecture:** `WindowRuntime` делится на GL-цель (окно или `OffscreenContext` — EGL surfaceless + pbuffer, перенос из RRiter) и оконную часть (`DirectWaylandBackend` или `HeadlessBackend`). Тело Wayland-цикла выносится в одну функцию шага, которую зовут и Wayland-цикл, и headless. Внешние эффекты App (буфер обмена, открытие ссылок, путь конфига) получают записывающие варианты, выбираемые при создании App.

**Tech Stack:** Rust nightly (edition 2024, `-Z build-std`), `glow` 0.18, `glutin` 0.32 (окно), `libc` (`dlopen` libEGL), Python 3 (`scripts/pgo_pipeline.py`). Новых зависимостей нет.

**Spec:** `/home/reyan/projects/ronsole/docs/superpowers/specs/2026-10-23-ronsole-headless-design.md` — этап 1 по §12 (Q28). Разделы спеки, на которые ссылаются задачи: §3.1–§3.4, §4.1, §8, §10, Q17, Q21, Q23, Q25, Q27.

## Global Constraints

* Wayland-путь без изменений поведения: тот же порядок шагов цикла (`src/app/direct_wayland.rs:143-223`), тот же порядок уничтожения GL-объектов (`Renderer` при живом контексте → surface/context → оконная часть → eventfd).
* Темп кадра headless — `HEADLESS_FRAME_PACE = 8 мс` (как `rriter/src/headless/frame.rs:22`).
* Размер `--pgo-train` по умолчанию — физический 1280x800, масштаб 1.0 (§4.1).
* Профиль — `${XDG_RUNTIME_DIR:-/tmp}/ronsole-headless-<pid>[-<n>]`, права 0700, удаляется при выходе (§3.4); на этапе 1 `--profile`/`--keep-profile` не реализуются.
* Коды выхода `--pgo-train`: `0` — отчёт `ok`; `1` — ошибка шага, таймаут, запись отчёта, ошибка сессии (Q25); `2` — неверные аргументы; `3` — ошибка EGL/`Renderer` со строкой `headless: EGL setup failed at <stage>: <detail>` в stderr и отчётом о сбое старта (`write_automation_startup_failure`).
* `--headless` без `--pgo-train` на этапе 1 — код 2, stderr `headless: protocol mode is not implemented yet`.
* Headless не открывает сокет single-instance и не подключается к Wayland даже при заданном `WAYLAND_DISPLAY` (Q17).
* Без глобального изменяемого состояния (`static` + `Mutex`/`OnceLock`/атомики с данными приложения); переменные окружения процесса не меняются.
* libEGL — `dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL | RTLD_NODELETE)`; `eglTerminate` не вызывается; без `static DISPLAY_USERS` (Q23).
* Правила Ronsole (`AGENTS.md`): без `cargo fmt` по всему репозиторию, без `cargo clean`; коммиты явными путями (`git add <file>…`); тесты — `make test TEST_FILTER=<filter>` или `make test-one TEST='mod::name'`; финальный гейт — `make codex_test` (только главная); `make api-map` перед сдачей ветки (главная); clippy `-D warnings`.
* Каждый тест — свой процесс (`-Z panic-abort-tests`); всё, что тест пишет вне процесса, уникально на процесс (`std::process::id()` в имени).
* Тесты с GL требуют Mesa EGL surfaceless: `make test` передаёт `TEST_EGL_VENDOR ?= mesa` в переменную вендора, которую читает `prefer_egl_vendor` (`src/main.rs:26-89`).

## Review Focus

1. Wayland-окно после рефакторинга шва ведёт себя как раньше: present, frame callback, resize, выход по крестику и `CloseTab`, порядок уничтожения — существующие тесты плюс ручной запуск пользователем (§11.5); ревьюер Task 2 и Task 4 сверяет, что ветки `Wayland` — перенос старого кода без изменений.
2. Кадр, запрошенный раньше 8 мс после прошлого, не теряется: цикл просыпается по сроку темпа, а не ждёт события (тест `headless_poll_wakes_at_frame_pace`, Task 2).
3. Профиль удаляется при любом штатном выходе, включая ошибку EGL и ошибку автоматизации (тест `headless_profile_removed_on_drop`, Task 4).
4. `XDG_RUNTIME_DIR` не задан или каталог занят — фолбэк `/tmp` и суффикс `-<n>` (тест `headless_profile_dir_fallback_and_suffix`, Task 4).
5. `--pgo-train` при заданном `WAYLAND_DISPLAY` не трогает Wayland и системный буфер (тест `headless_app_uses_memory_clipboard_and_recording_opener`, Task 3; разбор аргументов — Task 4).

## Файлы и общие интерфейсы

| Файл | Задача | Ответственность |
|---|---|---|
| `src/platform/offscreen_gl.rs` (новый) | 1 | `OffscreenContext`: EGL surfaceless + pbuffer |
| `src/platform.rs` | 1 | `pub(crate) mod offscreen_gl;` |
| `Makefile` | 1 | `TEST_EGL_VENDOR ?= mesa` в цели `test`/`test-one` |
| `src/headless/mod.rs` (новый), `src/headless/backend.rs` (новый) | 2 (backend), 4 (mod: вход `main`) | `HeadlessBackend`, темп; вход headless |
| `src/runtime.rs` | 2 | шов `GlTarget`/`RuntimeBackend`, `bootstrap_headless` |
| `src/input.rs`, `src/platform/process.rs`, `src/config.rs`, `src/app.rs` (поля и конструктор), `src/app/automation.rs` (конструктор) | 3 | эффекты и путь конфига |
| `src/app/direct_wayland.rs`, `src/app/headless_run.rs` (новый), `src/main.rs`, `src/app/automation.rs` (после Task 3) | 4 | функция шага, запуск headless, аргументы, короткий сценарий для теста |
| `src/main.rs`, `src/launch.rs`, `src/app/automation.rs`, `scripts/pgo_pipeline.py`, `Makefile` (после Task 1) | 5 (после 4) | удаление оконной тренировки, PGO-скрипт без Wayland |

Интерфейсы между задачами:

* Task 1 → 2: `pub(crate) struct OffscreenContext` с `new(width: u32, height: u32) -> Result<Self, String>`, `glow(&self) -> glow::Context`, `resize(&mut self, width: u32, height: u32) -> Result<(), String>`, `make_current(&self) -> Result<(), String>`, `size(&self) -> (u32, u32)`, `gl_strings(&self) -> &GlStrings`, `requested_context(&self) -> String` (как в RRiter, `offscreen_gl.rs:187`). Ошибка `new` — строка `EGL setup failed at <stage>: <detail>`; нулевой размер в `new` и `resize` — `Err`, `resize` при ошибке оставляет прежний pbuffer.
* Task 2 → 4: `WindowRuntime::bootstrap_headless(physical_width: u32, physical_height: u32, scale_factor: f32, terminal_font_size: f32, terminal_background: crate::config::RgbColor) -> Result<Self, String>`; `WindowRuntime::take_session_error(&mut self) -> Option<String>`; `#[cfg(test)] WindowRuntime::inject_session_error(&mut self, error: String)`; `#[cfg(test)] WindowRuntime::headless_frames(&self) -> Option<u64>`; `crate::headless::backend::HEADLESS_FRAME_PACE: Duration`.
* Task 3 → 4: `App::load_headless(options: Option<AutomationOptions>, profile_dir: &Path) -> Result<App, String>` — конфиг из `<profile_dir>/config`, буфер `Memory`, открытие ссылок в `OpenSink::Record` (без запуска `xdg-open`), автоматизация при `Some`. Тело — приватный `App::load_headless_with(automation: Option<AutomationController>, profile_dir: &Path) -> Result<App, String>` (`pub(super)`), `load_headless` строит контроллер из `options` и зовёт его; Task 4 подаёт в него тестовый контроллер.

## Setup (SDD)

Интеграционная ветка `headless-pgo` от свежего `main`. Задачи 1 и 3 независимы (нет общих файлов, кроме `PROJECT_AI_MAP.txt`, который перегенерирует pre-commit; конфликт только в нём при вливании — главная делает `make api-map` и коммитит карту, это не fix-раунд) — параллельно; Task 2 после Task 1; Task 4 после Task 2 и Task 3; Task 5 после Task 4; Task 6 — главная после вливания всех. Worktree задачи греет `target/` reflink-копией из основного чекаута до первой сборки: `mkdir -p target && cp -a --reflink=always <main>/target/x86_64-unknown-linux-gnu <main>/target/release target/` (то, что есть), иначе первая сборка с `-Z build-std` — десятки минут. Сборку в worktree разрешает главная в брифе; одновременно собирают не больше двух.

---

### Task 1: `OffscreenContext` — внеэкранный EGL

Агенты: implementer ds-low · reviewer ds-high — high: task review unsafe FFI EGL, Drop и RTLD_NODELETE

**Files:**
- Create: `src/platform/offscreen_gl.rs` (перенос `/home/reyan/projects/rriter/src/platform/offscreen_gl.rs`)
- Modify: `src/platform.rs` (объявление модуля), `Makefile` (цели `test`, `test-one`: вендор EGL)

**Interfaces:** Produces — блок «Task 1 → 2» выше.

Перенос с отличиями от RRiter (§3.2, Q23): тип называется `OffscreenContext`, как в RRiter; нет `new_traced` и `StartupTrace`; нет `static DISPLAY_USERS` и `eglTerminate`; `dlopen` с флагами `RTLD_NOW | RTLD_LOCAL | RTLD_NODELETE` (`libc::RTLD_NODELETE`), `Library::drop` по-прежнему зовёт `dlclose` — библиотека остаётся в памяти; `Drop for OffscreenContext`: `make_current(null)`, `destroy_context`, `destroy_surface`; новый метод `make_current(&self)` — `eglMakeCurrent(display, surface, surface, context)`, ошибка — строка с кодом `eglGetError`; ошибки `new` начинаются с префикса `EGL setup failed at ` (в RRiter его добавляет вызывающий, `rriter/src/headless/mod.rs:365`; у нас — сам `offscreen_gl`, формат остального `"<Stage>: eglGetError=…"` прежний); `new` и `resize` с нулевой шириной или высотой — `Err("CreatePbuffer: zero size")` до вызова EGL (RRiter размер не проверяет, `offscreen_gl.rs:172-175`; pbuffer 0x0 Mesa может создать); тестовый блок RRiter, зависящий от его типов, не переносится. Вендор: в `Makefile` `TEST_EGL_VENDOR ?= mesa`, передаётся в цели `test` и `test-one` как значение переменной окружения, которую читает `prefer_egl_vendor` (`src/main.rs:26-89` — имя переменной взять оттуда); тесты модуля вызывают `crate::prefer_egl_vendor()` до первого `OffscreenContext::new`.

- [ ] **Step 1: Тесты** в `#[cfg(test)] mod tests` файла `offscreen_gl.rs`:
  * `offscreen_context_reports_size_and_resizes`: `new(64, 48)` → `size() == (64, 48)`, `gl_strings().renderer` не пуст; `resize(128, 96)` → `Ok`, `size() == (128, 96)`.
  * `second_context_survives_first_drop` (Q23): `a = new(32, 32)`, `b = new(32, 32)`, `drop(a)`; `b.make_current()` → `Ok`; через `b.glow()` `clear_color(1,0,0,1)` + `clear(COLOR_BUFFER_BIT)` + `read_pixels` одного пикселя → `[255, 0, 0, 255]`.
  * `resize_failure_keeps_old_surface`: `resize(0, 0)` → `Err`, `size()` прежний, `make_current()` → `Ok`.
- [ ] **Step 2:** `make test TEST_FILTER=offscreen_gl` — не компилируется (модуля нет).
- [ ] **Step 3:** перенос и правки выше; `resize` создаёт новый pbuffer и уничтожает старый только при успехе.
- [ ] **Step 4:** `make test TEST_FILTER=offscreen_gl` — 3 passed.
- [ ] **Step 5:** commit `feat(headless): offscreen EGL context` (явные пути).

### Task 2: шов runtime и `HeadlessBackend`

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review шов WindowRuntime, Wayland-ветки без изменений, порядок Drop

**Files:**
- Create: `src/headless/mod.rs` (пока только `pub(crate) mod backend;`), `src/headless/backend.rs`
- Modify: `src/runtime.rs` (`WindowRuntime` 2653-2894), `src/main.rs` (`mod headless;`)

**Interfaces:** Consumes — Task 1. Produces — блок «Task 2 → 4».

Решения (§3.1, Q25):
* `WindowRuntime { renderer: Renderer, target: GlTarget, backend: RuntimeBackend, wake: WakeHandle }` — порядок полей = порядок уничтожения. `enum GlTarget { Window { … }, Offscreen(OffscreenContext) }` — в `Window` уходят `config`, `context`, `surface`, `requested_plan`, `requested_priority`, `swap_interval_applied` (поля, нужные только окну). `enum RuntimeBackend { Wayland(Box<DirectWaylandBackend>), Headless(HeadlessBackend) }`. Порядок уничтожения окна прежний: сейчас `context` объявлен раньше `surface` (`runtime.rs:2656-2657`) — сохранить фактический порядок, не «исправлять».
* `impl Drop for WindowRuntime`: только делает контекст цели текущим (для `Window` — ничего); поля дальше уничтожаются по порядку объявления.
* Один приватный `fn make_target_current(&self)` зовётся в начале каждого метода, который трогает GL: `render_terminal_and_present`, `resize`, `update_scale_factor`, `set_terminal_font_size`, `set_terminal_background`, `reset_fps_counter`, и в `Drop`; методы, которые трогают GL вне этого списка (`settings_background_cursor_from_x`, `terminal_search_cursor_from_x`, `runtime.rs:2842`, `:2884`), на этапе 1 headless не зовёт — не трогать. Для `Window` — пусто. Ошибку `make_current` у offscreen пишет в `session_error`.
* `HeadlessBackend` (в `backend.rs`): `metrics: WaylandWindowMetrics` (физический = размер pbuffer, логический = физический / масштаб, `runtime.rs:800`), `frame_requested: Cell<bool>`, `last_frame: Cell<Option<Instant>>`, `#[cfg(test)] frames: u64`, `session_error: Option<String>`. Поля этапа 2 (счётчик запросов кадра, форма курсора) не вводить: clippy `-D warnings` (`AGENTS.md:232`) отклонит непрочитанные поля как `dead_code`, а `#[allow]` запрещён (`AGENTS.md`). Чистые функции без GL: `fn frame_ready(&self, now: Instant) -> bool` (запрошен и `last_frame` нет или прошло ≥ `HEADLESS_FRAME_PACE`); `fn poll_timeout(&self, timeout_ms: i32, now: Instant) -> i32` — минимум переданного (`-1` = без срока, как `poll_timeout_millis`, `runtime.rs:278`) и остатка до `last_frame + 8 мс`, если кадр запрошен и не готов (остаток округляется вверх до мс).
* Точки шва для `Headless` (таблица §3.1): `wayland_metrics` — `metrics`; `request_frame` — `frame_requested = true`; `wayland_frame_ready_requested` — `frame_ready(now)`; `take_wayland_frame_ready` — то же и сброс `frame_requested`; `render_terminal_and_present` — рендер тем же кодом, без `swap_buffers`, включая `record_presented_frame` при `show_fps` (`runtime.rs:2821-2823`), затем `frames += 1` (`#[cfg(test)]`), `last_frame = now` и то, что для окна делает `mark_presented`, если от него зависит автоматизация (`WaitPresentedFrame`); `poll_wayland(t)` — `poll(2)` только по eventfd с `poll_timeout(t, now)`, тот же `WaylandPollOutcome`; `take_wayland_events` — пустой `WaylandRuntimeEvents`; `wayland_input_deadline` — `None`, `process_wayland_input_deadline` — ничего; `acknowledge_wake`, `wake_handle` — как у окна; `set_cursor_kind` — ничего; `activate_existing_window` — ничего; `resize(w, h)` — если `target.size() != (w, h)`: `OffscreenContext::resize`, при ошибке `renderer.resize` не зовётся, в `session_error` — `format!("resize pbuffer {w}x{h}: {e}")`; при успехе (или равном размере) `renderer.resize` и `metrics`; `update_scale_factor` — масштаб в `metrics`, логический пересчитан, физический прежний; `diagnostics_report` — плюс строка `headless pbuffer <w>x<h> <requested_context>`.
* `bootstrap_headless(...)`: `OffscreenContext::new` → `Renderer::new` с теми же 4 аргументами, что в `bootstrap_wayland` (`renderer.rs:486-491`; GL — `target.glow()`) → шрифт/фон, `renderer.resize` и `trim_allocator_after_gl_bootstrap` как в `bootstrap_wayland` (`runtime.rs:2706`, `:2720`) → `WakeHandle::new`. Ошибка `Renderer` — та же форма строки `EGL setup failed at renderer: <detail>`.
* `take_session_error` — `Option::take` у headless, `None` у окна.

- [ ] **Step 1: Тесты.** Чистые, в `backend.rs`: `frame_ready_respects_pace` (запрос сразу после кадра → `false`; через 8 мс → `true`; без прошлого кадра → `true`); `poll_timeout_caps_infinite_wait_at_pace` (`poll_timeout(-1, last+3мс)` при запрошенном кадре → `5`; без запроса → `-1`; `poll_timeout(2, …)` → `2`). С GL, в `runtime.rs` `#[cfg(test)]` (вызвать `crate::prefer_egl_vendor()`): `headless_runtime_renders_frame` (`bootstrap_headless(320, 200, 1.0, 14.0, <фон по умолчанию>)`, `request_frame`, `take_wayland_frame_ready` → `true`, `render_terminal_and_present` с пустыми параметрами (как зовёт `App::on_frame`) → `Ok`, `headless_frames() == Some(1)`); `headless_poll_wakes_at_frame_pace` (после кадра `request_frame`, `take_wayland_frame_ready` → `false`, `poll_wayland(-1)` возвращается за < 100 мс с `timed_out`, затем `take_wayland_frame_ready` → `true`); `headless_resize_changes_pbuffer` (`resize(640, 400)` → `wayland_metrics()` физический `640x400`, `take_session_error() == None`); `headless_resize_failure_sets_session_error` (`resize(0, 0)` → метрики прежние, `take_session_error()` — `Some`).
- [ ] **Step 2:** `make test TEST_FILTER=headless` — не компилируется.
- [ ] **Step 3:** реализация по решениям выше; ветки `Wayland` — перенос старого тела метода без изменений.
- [ ] **Step 4:** `make test TEST_FILTER=headless` — 6 тестов Step 1 зелёные (сверять по именам: после Task 3 фильтр захватит ещё `headless_app_…`); `make test TEST_FILTER=runtime` — существующие тесты runtime зелёные.
- [ ] **Step 5:** commit `feat(headless): runtime seam with offscreen target`.

### Task 3: записывающие эффекты и путь конфига

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review маршрутизация буфера обмена и открытия ссылок, Wayland-поведение без изменений

**Files:**
- Modify: `src/input.rs` (`TerminalInteraction` 15-60, `clipboard*` 339-354, обращения 438-556; `#[ignore]` тест 1674), `src/platform/process.rs` (349-400), `src/config.rs` (87-110, 175-177), `src/app.rs` (поля, `from_config`, `load` 416-421, вызов `open_uri` 1785-1789), `src/app/automation.rs` (`load_with_automation` 1556)

**Interfaces:** Produces — блок «Task 3 → 4».

Решения (§3.4, Q27):
* Буфер обмена: `enum ClipboardOwner { System(Option<Clipboard>), Memory { text: Option<String> } }` вместо `clipboard: Option<Clipboard>`. `System` — ленивое создание `arboard`, как сейчас (`input.rs:339-343`). `clipboard_text()`/`set_clipboard_text()` и чтение списка файлов идут через владельца; `Memory`: запись сохраняет текст, `get_text` — последняя запись или `None`, список файлов — `None`. Все обращения из `input.rs`, `terminal.rs:1743-1744`, `app.rs:1058,1070` идут через эти методы, прямых `Clipboard` вне владельца нет. Конструктор `TerminalInteraction::new_with_memory_clipboard()` (или параметр существующего — выбрать по месту).
* Открытие ссылок: `pub(crate) enum OpenSink { System, Record(std::sync::mpsc::Sender<String>) }`; `pub fn open_uri_with(uri: &str, sink: OpenSink)` — тот же поток и то же решение O2 (`opener_argument`), в конце `run_xdg_open(&argument)` для `System` или `sender.send(argument)` для `Record` (ошибка `send` при брошенном приёмнике игнорируется); `open_uri(uri)` = `open_uri_with(uri, OpenSink::System)`. App держит `open_sink: OpenSink` (клонируется на вызов). Приёмник на этапе 1 в App не хранится (его читает только протокол этапа 2; непрочитанное поле отклонит clippy `-D warnings`, `#[allow]` запрещён): `load_headless` создаёт канал и бросает `Receiver`.
* Конфиг: `AppConfig` хранит путь, откуда загружен (несериализуемое поле); `AppConfig::load_at(path: PathBuf) -> Self`; `load()` = `load_at` от `config_path()` (поведение без изменений при `None`); `save()` пишет по сохранённому пути.
* `App::load_headless(options, profile_dir)`: `AppConfig::load_at(profile_dir.join("config"))`, буфер `Memory`, `OpenSink::Record`, автоматизация через тот же `AutomationController::new(options)`, что `load_with_automation`, — в `load_headless`, остальное в `load_headless_with` (интерфейс «Task 3 → 4»). Общий код с `load`/`load_with_automation` не копируется.

- [ ] **Step 1: Тесты.** `memory_clipboard_round_trip` (`input.rs`): до записи `clipboard_text() == None`; `set_clipboard_text("x")` → `true`, затем `Some("x")`; список файлов — `None`. Снять `#[ignore = "no clipboard seam …"]` с теста `input.rs:1674` и заставить его идти через `Memory`-буфер. `record_sink_receives_opener_argument` (`process.rs`): `open_uri_with("https://example.com/a", Record(tx))` → `rx.recv_timeout(2 с) == "https://example.com/a"`; исполняемый файл `run.sh` (0755) в `std::env::temp_dir()/ronsole-open-<pid>` → `file://<этот каталог>` (решение O2, `process.rs:367-397`). `config_load_at_saves_to_same_path` (`config.rs`): `load_at(<tmp-<pid>>/config)` на несуществующем файле — значения по умолчанию; изменить поле, `save()` → файл появился по этому пути. `headless_app_uses_memory_clipboard_and_recording_opener` (`app.rs` тесты): `App::load_headless(None, <tmp-<pid>>)` → буфер `Memory` (через `#[cfg(test)]`-аксессор `TerminalInteraction`, поле `clipboard` приватно, `input.rs:19`), `open_sink` — `OpenSink::Record(_)`.
- [ ] **Step 2:** `make test TEST_FILTER=clipboard` и остальные фильтры по именам — красные/не компилируются.
- [ ] **Step 3:** реализация по решениям выше.
- [ ] **Step 4:** `make test TEST_FILTER=input`, `TEST_FILTER=process`, `TEST_FILTER=config`, `TEST_FILTER=app::` — зелёные, включая снятый с `#[ignore]`.
- [ ] **Step 5:** commit `feat(headless): recording clipboard, opener and profile config`.

### Task 4: функция шага и запуск `--headless --pgo-train`

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review вынос тела Wayland-цикла и коды выхода

**Files:**
- Modify: `src/app/direct_wayland.rs` (`run_direct_wayland_inner` 25-65, `direct_wayland_loop` 139-224), `src/main.rs` (ветвление на `headless::main` после 126-128), `src/app/automation.rs` (общий `with_steps` из `new` 572-600, тестовый конструктор), `src/headless/mod.rs`, `src/app.rs` (`mod headless_run;`)
- Create: `src/app/headless_run.rs`

**Interfaces:** Consumes — Task 2 (`bootstrap_headless`, `take_session_error`, `inject_session_error`), Task 3 (`App::load_headless`, `App::load_headless_with`).

Решения (§3.3, §4.1, §8, §10, Q21, Q25):
* `pub(super) enum LoopStep { Continue, Exit(Result<(), String>) }`; `pub(super) fn loop_step(&mut self, external_launches: Option<&Receiver<InstanceMessage>>) -> Result<LoopStep, String>` — тело итерации `direct_wayland.rs:143-223` без изменений порядка; `drain_external_launches` — только при `Some`. В начале шага: `if let Some(error) = runtime.take_session_error()` → `interrupt_automation(&error)` (причина начинается с `resize pbuffer`, если ошибка из `resize`), `prepare_exit`, `Exit(take_automation_exit_result())`. `direct_wayland_loop` = цикл по `loop_step(Some(external_launches))`.
* Подготовка после bootstrap (`on_runtime_ready`, начальный `on_resize_with_logical_size`, `direct_wayland.rs:36-44`) — один `pub(super)` helper для Wayland и headless (`loop_step` и helper живут в `app::direct_wayland`, зовутся из `app::headless_run`).
* `src/app/headless_run.rs`: `pub(crate) enum HeadlessRunError { Egl(String), Run(String) }`; `impl App { pub(crate) fn run_headless(&mut self, physical: (u32, u32), scale: f32, initial_launch: TerminalLaunchSpec) -> Result<(), HeadlessRunError> }` — `bootstrap_headless` (ошибка → `Egl`), helper подготовки (тот же параметр `initial_launch`, его отдаёт `on_runtime_ready`, `direct_wayland.rs:38-43`, `app.rs:1469`, `:1479`; Wayland передаёт свой, как сейчас), цикл `loop_step(None)`, в конце `on_exiting` (как `run_direct_wayland`, `direct_wayland.rs:21`); ошибка цикла — `interrupt_automation` и `Run`.
* `src/headless/mod.rs`: `pub(crate) fn main(args: &[OsString]) -> ExitCode`. `fn parse_args(args: &[OsString]) -> Result<AutomationOptions, String>`: `args[1] == "--headless"`; без `--pgo-train` — `Err("protocol mode is not implemented yet")` (код 2); иначе args без `--headless` → `automation_options_from_args` (отказ на неизвестных остаётся, код 2). `struct Profile { dir: PathBuf }` — `Profile::create(runtime_dir: Option<PathBuf>, pid: u32) -> io::Result<Profile>`: база — `runtime_dir` или `/tmp`, имя `ronsole-headless-<pid>`, при занятости `-1`, `-2`…, `create_dir` с правами 0700; `Drop` удаляет каталог рекурсивно. Порядок `main`: разбор → `Profile::create(env XDG_RUNTIME_DIR)` → `crate::prefer_egl_vendor()` → `crate::tune_glibc_allocator()` → `App::load_headless(Some(options), &profile.dir)` → `run_headless((1280, 800), 1.0, TerminalLaunchSpec::default())` (шелл по умолчанию, §12). Коды: `Ok` → `0`; `Egl(e)` → stderr `headless: EGL setup failed at …` (строка из `e`), `write_automation_startup_failure(&options, "headless-egl", &e)`, `3`; `Run`/ошибка `load_headless` → `1`; ошибка `Profile::create` → stderr `headless: profile: <e>`, `write_automation_startup_failure(&options, "headless-profile", &e.to_string())` (`automation.rs:43-47`), `1`.
* Короткая автоматизация для теста (§12): `AutomationStep`/`SemanticBlock` приватны в `app::automation` (`automation.rs:147`, `:262`), поэтому список шагов собирается там же. Проверки и создание из `new` (`automation.rs:572-600`) выносятся в приватный `fn with_steps(options, steps)`; `new` = `with_steps(options, scenario())`; `#[cfg(test)] pub(super) fn with_short_scenario(options: AutomationOptions) -> Result<Self, String>` = `with_steps` со списком `WaitReady, WaitPresentedFrame, Complete(B)` для каждого `B` из `SemanticBlock::ALL` по порядку (`:165-180`, `Finish` проверяет порядок, `:1469-1471`), `CleanupBarrierFinal` (ставит `terminal_cleanup_finalized`, `:990-993`, `:1011`), `Finish`. `WaitReady` ждёт `presentation_ready` до 30 с (`:28`, `:1028-1036`) — его ставит первый непробельный вывод шелла (`ronsole-term/src/perform.rs:53-54`): тест передаёт в `run_headless` `TerminalLaunchSpec::hermetic_shell_for_test()` (`launch.rs:174`, bash -i печатает приглашение; `$SHELL` процесса не менять — Global Constraints; что приглашение приходит до 30 с, не проверено — не приходит, NEEDS_CONTEXT). Раскладка 320x200 должна дать `cols>=10`, `visible_rows>=2` (`:1030-1031`) — не проверено, при нехватке взять больший размер.
* `src/main.rs`: сразу после `tab-status` (`main.rs:126-128`), до `parse_startup_args_with_current_dir` и `acquire_single_instance`: `if args.get(1).is_some_and(|a| a == "--headless") { return headless::main(&args); }`.

- [ ] **Step 1: Тесты.** В `headless/mod.rs`: `headless_args_without_pgo_train_rejected` (`["ronsole","--headless"]` → `Err` с `protocol mode is not implemented yet`); `headless_args_pass_pgo_options` (`["ronsole","--headless","--pgo-train","--pgo-workspace","/w","--pgo-report","/r","--pgo-timeout-seconds","5"]` → `workspace == "/w"`, `timeout == 5 с`); `headless_args_unknown_flag_rejected` (`… --pgo-train --script x` → `Err`); `headless_profile_dir_fallback_and_suffix` (`create(None, pid)` → под `/tmp`, права 0700; второй `create` с тем же pid → суффикс `-1`); `headless_profile_removed_on_drop` (каталог с файлом внутри после `drop` не существует). В `headless_run.rs` (GL, `crate::prefer_egl_vendor()`): `headless_session_error_fails_automation` — workspace `tmp-<pid>` с пустым `terminal_fixture.sh` (без него `AutomationController::new` вернёт `Err`, `automation.rs:573-588`), `App::load_headless(Some(options), profile)`, `bootstrap_headless`, helper подготовки, `inject_session_error("resize pbuffer: test")`, один `loop_step(None)` → `Exit(Err(_))`, файл отчёта содержит статус `failed` и причину `resize pbuffer: test`. `headless_short_automation_reaches_ok` (§12) — тот же workspace, `App::load_headless_with(Some(AutomationController::with_short_scenario(options).expect(…)), profile)`, `run_headless((320, 200), 1.0, TerminalLaunchSpec::hermetic_shell_for_test())` → `Ok`, отчёт со статусом `ok`, тест укладывается в таймаут `options` 30 с. В source-order тест `main.rs` (по образцу `main.rs:252-273`) добавить проверку Q17: в тексте `main.rs` `headless::main(&args)` стоит раньше `parse_startup_args_with_current_dir` и `acquire_single_instance`.
- [ ] **Step 2:** `make test TEST_FILTER=headless` — красный.
- [ ] **Step 3:** реализация по решениям выше.
- [ ] **Step 4:** `make test TEST_FILTER=headless`, `make test TEST_FILTER=direct_wayland`, `make test TEST_FILTER=automation` — зелёные; тесты `main.rs` — `make test-one TEST='tests::startup_cli_classification_precedes_single_instance_and_wayland_startup'` и `make test-one TEST='tests::single_instance_gate_precedes_direct_wayland_app_loop'` (`--exact`, `Makefile:129`; фильтр `main` тесты `main.rs` не выбирает) — зелёные.
- [ ] **Step 5:** commit `feat(headless): pgo-train run without Wayland`.

### Task 5: удаление оконной тренировки и PGO-скрипт без Wayland

Агенты: implementer ds-low-agentic · reviewer ds-high — high: task review удаление оконной тренировки, разбор `-e` и обычный запуск без изменений

**Files:**
- Modify: `src/main.rs` (132-200, тест 252-273), `src/launch.rs` (17-20, 59-92, 134-146, хелпер тестов 201, тесты 300-347, 355-367 и 370-383), `src/app/automation.rs` (`load_with_automation` 1556-1560), `src/app.rs` (реэкспорт 35 — проверить нужность), `scripts/pgo_pipeline.py` (418-438, 454-486, 570-580, 679, 1138-1139, 1179-1180, `self_test` 1279, блоки 1394-1470 и 1505-1518), `Makefile` (echo-строки 186, 200, 204, 209, 213)

**Interfaces:** Consumes — Task 4 (`--headless --pgo-train` работает).

Решения (Q5, §8, §10):
* Оконная тренировка удаляется (Q5): `StartupMode::PgoAutomation` и ветка выбора режима в `parse_startup_args_with_current_dir` (`launch.rs:19`, `:88-90`; `pgo_automation_requested` не удаляется, а переименовывается — ниже), ветка `main.rs:132-141`, отчёт о сбое для вторичного экземпляра в режиме обучения (`main.rs:149-177`, только его часть про обучение), `App::load_with_automation` (единственный вызывающий — `main.rs:183`). `automation_options_from_args` и `write_automation_startup_failure` остаются (нужны headless). Тест `main.rs:252-273` (`startup_cli_classification_…`): искомую строку `app::automation_options_from_args(&args)` заменить на `pgo_train_without_headless(&args)` (иначе `find` найдёт литерал в самом тесте, `main.rs:261`, `:271`), порядок `headless::main(&args)` < `pgo_train_without_headless(&args)` < `parse_startup_args_with_current_dir` < `acquire_single_instance`, проверку Q17 из Task 4 сохранить. Тест `launch.rs:355-367` и хелпер тестов `parse_startup` (`launch.rs:201`) — под возврат `TerminalLaunchSpec`; тесты `automation.rs:1605`, `:1654` не трогать.
* `--pgo-train` без `--headless` первым аргументом: функция в `launch.rs` `pub(crate) fn pgo_train_without_headless(args: &[OsString]) -> bool` — переименованная `pgo_automation_requested` (`launch.rs:68-79`) с её разбором без изменений: значения опций (`--workdir <dir>`) пропускаются через `normal_cli_token`, после `-e` всё — команда шелла (`launch.rs:59-63`, `:73`, `:134-146`); `main.rs` вызывает её после ветки `headless::main` (Task 4) и перед `parse_startup_args_with_current_dir` — `--headless` разбор считает обычным `Other` (`launch.rs:65`, `:75`), поэтому раньше ветки `headless::main` проверка отклонила бы `--headless --pgo-train`; при `true` печатает `ronsole: --pgo-train requires --headless` и выходит с кодом 2 (неверные аргументы, Global Constraints). `ronsole -e foo --pgo-train` и `ronsole --workdir --pgo-train` — обычный запуск, как сейчас. `enum StartupMode` (`launch.rs:17-20`) с одним вариантом удаляется: `parse_startup_args_with_current_dir` возвращает `TerminalLaunchSpec` (в той же обёртке `Result`, что сейчас), вызывающие — `main.rs` и тесты `launch.rs`. Тест `launch.rs:370-383` (ждёт `StartupMode::PgoAutomation`) переделать на `pgo_train_without_headless(<тот же argv>) == true`; проверки разбора `--pgo-*` не удалять.
* `scripts/pgo_pipeline.py` (§8): удалить `resolve_wayland_socket` и все его вызовы (`:679`, `:1139`, `:1180`), строки `log_stage("preflight Wayland …")` (`:1138`, `:1179`) и требование `WAYLAND_DISPLAY`; в `isolated_training_environment` не передавать `WAYLAND_DISPLAY` (удалить ключ из окружения, если он пришёл из `base_environment`); `training_command` — `[exe, "--headless", "--pgo-train", "--pgo-workspace", …]`. Тесты скрипта — `python3 scripts/pgo_pipeline.py --self-test` (`self_test`, `pgo_pipeline.py:1279`; каталога `tests/` нет): в блоке 1394-1470 убрать проверки сокета и `WAYLAND_DISPLAY`, проверки приватного `XDG_RUNTIME_DIR` и изоляции `HOME`/`SHELL` оставить; проверку контракта команды `:1505-1518` (`child_command[1:]`) обновить под `--headless`. `Makefile`: echo-строки 186, 200, 204, 209, 213 не упоминают окно/Wayland/GUI.

- [ ] **Step 1: Тесты.** В `launch.rs`: `pgo_train_without_headless_detected` (`["ronsole","--pgo-train"]` → `true`; `["ronsole","-e","foo","--pgo-train"]` → `false`; `["ronsole","--workdir","--pgo-train"]` → `false`; `["ronsole"]` → `false`); существующие тесты `-e` (`launch.rs:300-347`) зелёные; меняется только снятая обёртка `StartupMode::Normal(…)`, ожидаемые `TerminalLaunchSpec` те же. В `self_test`: `training_command` содержит `--headless` вторым элементом, окружение тренировки без `WAYLAND_DISPLAY` при заданном во внешнем окружении.
- [ ] **Step 2:** `make test TEST_FILTER=launch` — не компилируется.
- [ ] **Step 3:** реализация по решениям выше.
- [ ] **Step 4:** `make test TEST_FILTER=launch`, `make test TEST_FILTER=automation`, два `make test-one` для тестов `main.rs` как в Task 4 Step 4 — зелёные; `python3 scripts/pgo_pipeline.py --self-test` — код 0.
- [ ] **Step 5:** один прогон полного сценария на собранном бинаре (`make pgo-script` бинарь не собирает, workspace с `terminal_fixture.sh` создаёт скрипт): `make fast > /tmp/ronsole-t5-fast.log 2>&1`, затем `timeout 540 env -u WAYLAND_DISPLAY make pgo-script PGO_AUTOMATION_TIMEOUT=300 > /tmp/ronsole-t5-pgo.log 2>&1` → код 0, отчёт `ok`, окно не появилось. Удаление профиля проверяет `headless_profile_removed_on_drop`: скрипт даёт процессу приватный `XDG_RUNTIME_DIR` и удаляет его сам. Не прошёл или упёрся в `timeout` — отчёт с `tail -n 30` лога и отчётом PGO, без повторов.
- [ ] **Step 6:** commit `feat(headless): pgo pipeline without Wayland, drop windowed training`.

### Task 6: приёмка этапа 1 и установка PGO-бинаря (главная)

Агенты: главная сессия

- [ ] Финальное ревью ветки `ds-high` (`high: final review headless seam`) + `final-reviewer`, правки по находкам.
- [ ] `make api-map`; `make codex_test` — зелёный (лог в файл); clippy с `-D warnings` командой из `AGENTS.md:232` — чисто (`codex_test` clippy не запускает, `Makefile:73-75`).
- [ ] `env -u WAYLAND_DISPLAY make pgo-auto` (`Makefile:199-201`, полный цикл `--mode fresh`, лог в файл, в фоне) — PGO-сборка без окна на экране (§11.2); окно не появилось.
- [ ] Ручная проверка пользователем: обычный Ronsole в окне не изменился (§11.5) — снимок/вопрос пользователю.
- [ ] PR в `main`, merge; установка PGO-бинаря в `~/bin/system/ronsole` (Q7) после merge.
