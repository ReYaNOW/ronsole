# Ronsole: сетка Claude — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** по Ctrl+Shift+S все вкладки, где на переднем плане Claude Code, показываются плиткой живых терминалов; ядро «список панелей» потом переиспользуют сплиты.

**Architecture:** App перед кадром и событием указателя собирает список панелей «терминал → прямоугольник» (обычный режим — одна панель, сетка — N). Рендер рисует тело для каждой панели одной функцией тела с прямоугольником аргументом и возвращает раскладку каждой панели; мышь выбирает панель по точке. Признак «вкладка с Claude» считает поток заголовков сессии на каждом тике.

**Tech Stack:** Rust (nightly через Makefile), glow/EGL, Wayland, `/proc`.

**Spec:** `docs/superpowers/specs/2026-10-28-ronsole-claude-grid-design.md` (утверждена 28.10). Номера §, Q и D ниже — из спеки; исполнитель читает её разделы, на которые ссылается задача.

## Global Constraints

- AGENTS.md §3: в кадре и в обработке вывода — без файлового ввода-вывода, `/proc`, `format!` и растущих аллокаций; списки панелей, раскладок, заголовков плиток — поля долгоживущего состояния, `clear()` и повторное использование.
- AGENTS.md §3: рендер только при dirty или активной анимации; сетка в простое не рисует; дедлайны — реальные `Instant` в `AppLoopControl::WaitUntil`, без опроса.
- AGENTS.md §4: все границы плиток, заголовков, рамок и тел — целые пиксели, одним хелпером; текст, курсор, выделение и hitbox одной строки — одна геометрия.
- Вне сетки поведение Ronsole не меняется (спека §1): все существующие тесты остаются зелёными без правки ожиданий, кроме тестов на структуру исходника (`src/app.rs:2862-2938`, `:4653-4687`, `src/runtime.rs:3763-3778`), которые обновляются в той же задаче, что меняет проверяемый ими текст.
- Терминальное поведение крейта `crates/ronsole-term` не меняется; новых зависимостей нет.
- После правки любого `.rs` — `make api-map`, обновлённый `PROJECT_AI_MAP.txt` в том же коммите.
- Тесты: одна функция — `make test-one TEST='<module>::tests::<name>'`; группа — `make test TEST_FILTER=<filter>`. Не `cargo clean`. Коммиты явными путями (`git add <file>…`).
- Тексты подсказок константные, ровно: «Нет вкладок с Claude», «Слишком много вкладок с Claude для окна».

## Review Focus

- Окно меняет размер или scale, пока сетка открыта, и пользователь кликает до нового кадра: клик не попадает в ячейку соседней плитки (поколение растёт в `set_grid_metrics`, Task 4; проверка поколения, Task 5).
- Claude завершается в плитке в фокусе, пока пользователь печатает: фокус переходит на соседнюю плитку, 1004 уходит парой out/in, отложенная активация старой вкладки фокус не перехватывает (Task 4).
- Ctrl+Shift+S при открытых настройках или диалоге закрытия ничего не делает; Ctrl+Shift+T и внешний запуск из сетки выходят из неё (Task 4).
- Дробный scale 1.25 и 1.5: границы плиток целые, плитки не перекрываются и не оставляют щелей сверх промежутка (Task 2).
- Сетка в простое: после входа и успокоения анимаций `animation_active == false`, цикл ждёт `Wait`/реальный дедлайн (Task 6).

---

### Task 1: Признак «на переднем плане Claude Code» у сессии

**Files:**
- Modify: `src/terminal_process.rs` (`TerminalTitleState` :42-49, `refresh_terminal_title_cache` :787-843, `install_terminal_title_refresh` :845-878)
- Modify: `src/terminal.rs` (аксессор рядом с `write_display_title` :2193)
- Modify: `src/tab_input.rs` (тестовые хелперы :421-480)
- Test: тесты в `src/terminal_process.rs` и `src/tab_input.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: concurrency поток заголовков и main-поток через mutex и wake: флаг на каждом тике независимо от ранних выходов, будение только на смене

**Interfaces:**
- Produces: `Terminal::claude_foreground(&self) -> bool`; `#[cfg(test)] Terminal::set_claude_foreground_for_test(&self, value: bool)`; `#[cfg(test)] pub(crate) mod test_support` в `src/tab_input.rs` с `TestDir`, `TestDir::new`, `TestDir::shell_named`, `spawn_shell`, `Reaper` с `pub(crate)` доступом к pid (перенос существующих хелперов, тесты `tab_input` переходят на них) и новым `TestDir::binary_named(&self, source: &Path, name: &str) -> PathBuf` — копия произвольного бинарника под именем (`shell_named` — его частный случай над `/bin/sh`, если так устроен сейчас); их использует Task 7.
- Эталон «подставного Claude» для тестов с живой оболочкой (Task 1 связка цикла, Task 7): копия `/usr/bin/cat` под именем `claude` (`binary_named`), запущенная оболочкой вкладки без аргументов: читает PTY, остаётся лидером группы переднего плана с `comm == "claude"` и не читает rc-файлов. Копию `/bin/sh` как оболочку вкладки не использовать: `/bin/sh` здесь `bash`, без аргументов он интерактивный и читает `~/.bashrc` (нет изоляции; эталон изоляции — `hermetic_shell_for_test`, `src/launch.rs:170-177`). Перед первым запуском копии — один запуск `<copy> --version` с тем же ретраем ETXTBSY, что в `spawn_shell`.

Решения (§4.2):
- `TerminalTitleState` получает поле `claude_foreground: bool` и метод `set_claude_foreground(&mut self, value: bool) -> bool` (возвращает «изменилось»).
- Группа переднего плана читается один раз на тик: вынести `fn foreground_process_group(master_pty) -> Option<u32>` из начала `refresh_terminal_title_cache`; функция получает `process_group: Option<u32>` параметром вместо чтения сама.
- Новая `fn observe_claude_foreground(title_cache: &TerminalTitleCache, process_group: Option<u32>) -> bool`: `process_group.and_then(crate::tab_input::claude_generation).is_some()` → `set_claude_foreground`. Нет группы — флаг снят.
- Цикл в `install_terminal_title_refresh`: `let group = foreground_process_group(..)`; `let title_changed = refresh_terminal_title_cache(.., group, ..)`; `let claude_changed = observe_claude_foreground(&title_cache, group)`; `wake.wake()` при `title_changed || claude_changed`. Вызов `observe_claude_foreground` не зависит от ранних `return false` заголовка.
- Новых таймеров и обходов `/proc` нет; `claude_generation` — те же два чтения (`comm`, `stat`).

- [ ] **Step 1: тесты (красные)**
  - `observe_claude_foreground_tracks_claude_leader_and_reports_only_changes`: подставной `claude` (`dir.shell_named("claude")` у `TestDir`, `spawn_shell(&claude, "sleep 30; true")`), ограниченное ожидание до 2 с (цикл по 10 мс), пока `claude_generation(pid)` не станет `Some`; затем `observe_claude_foreground(&cache, Some(pid)) == true` и флаг `true`; повтор с тем же pid — `false`, флаг `true`; `Some(std::process::id())` — `true`, флаг `false`; `None` — `false` (уже снят).
  - `set_claude_foreground_reports_change`: `false→true` → `true`, `true→true` → `false`.
  - `claude_foreground_set_by_title_thread_for_claude_shell` (связка цикла, §6 «вычисляется на тике без изменения снимка»): `Terminal::spawn` с оболочкой — подставным Claude (копия `/usr/bin/cat`, см. эталон в Interfaces; оболочку задаёт `TerminalLaunchSpec.shell`; образец сборки — `session_with_shell`, `src/headless/tests.rs:29`, и `shell_override`, `src/app.rs:762`). Ограниченное ожидание до 3 с (цикл по 50 мс), пока `terminal.claude_foreground()` не станет `true`; затем `set_claude_foreground_for_test(false)` и снова ограниченное ожидание до 3 с возврата `true` — этот тик идёт с неизменным снимком группы (`last_process_group` уже тот же), поэтому тест падает, если `observe_claude_foreground` вызывается только в ветке перехода.
- [ ] **Step 2:** `make test TEST_FILTER=claude_foreground` — FAIL (нет функций).
- [ ] **Step 3:** реализация по решениям выше; перенос хелперов в `test_support`.
- [ ] **Step 4:** `make test TEST_FILTER=claude_foreground` и `make test TEST_FILTER=tab_input` — PASS.
- [ ] **Step 5:** `make api-map`; коммит `src/terminal_process.rs src/terminal.rs src/tab_input.rs PROJECT_AI_MAP.txt`.

### Task 2: Чистая геометрия: размер тела, раскладка сетки, выбор панели

**Files:**
- Create: `src/panels.rs` (модуль в `src/main.rs` рядом с `mod tabs;`)
- Modify: `src/renderer/terminal_ui.rs` (вынос расчёта cols/rows из `render_terminal_body` :3482-3488; `terminal_body_rect` :189 становится `pub(crate)`)
- Test: `src/panels.rs`, `src/renderer/terminal_ui.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review snapped-геометрия плиток при дробном scale и порог Q9 по сырому расчёту

**Interfaces:**
- Produces (`src/renderer/terminal_ui.rs`): `pub(crate) struct TerminalGridSize { pub cols: usize, pub rows: usize, pub raw_cols: f32, pub raw_rows: f32 }`; `pub(crate) fn terminal_raw_rows(content_h: f32, char_h: f32, scale: f32) -> f32` = `terminal_text_viewport_height(content_h, scale) / char_h.max(0.0001)` — его используют и `terminal_visible_rows` (`.floor().max(2.0)`), и `terminal_grid_size`; `pub(crate) fn terminal_grid_size(body: Rect, char_w: f32, char_h: f32, scale: f32) -> TerminalGridSize` — `raw_cols = (body.w - 20.0 * scale) / char_w`, `raw_rows = terminal_raw_rows(body.h, char_h, scale)`, `cols = raw_cols.floor().max(10.0) as usize`, `rows = raw_rows.floor().max(2.0) as usize` (те же формулы, что сейчас в :3487 и :1384-1387); `render_terminal_body` использует его.
- Produces (`src/panels.rs`):
  - `pub(crate) struct Panel { pub terminal: usize, pub body: Rect, pub header: Option<Rect>, pub content_hidden: bool }` (`header: None` — обычный режим; `Some` — плитка);
  - `pub(crate) const GRID_TILE_GAP: f32 = 6.0;` `pub(crate) const GRID_TILE_HEADER: f32 = 22.0;` (логические пиксели × scale, `.round()`);
  - `pub(crate) fn grid_tiles(body: Rect, n: usize, scale: f32, out: &mut Vec<(Rect, Rect)>)` — пары (заголовок, тело) в порядке плиток; `out.clear()` в начале;
  - `pub(crate) fn grid_fits(body: Rect, n: usize, scale: f32, char_w: f32, char_h: f32) -> bool` — для каждого тела плитки `raw_cols >= 10.0 && raw_rows >= 2.0`; `n == 0` — `true`;
  - `pub(crate) enum PanelHit { Header(usize), Body(usize) }` и `pub(crate) fn panel_hit(panels: &[Panel], x: f32, y: f32) -> Option<PanelHit>` (индекс — в `panels`; промежутки — `None`).

Алгоритм `grid_tiles` (§4.3 «Раскладка»): `cols = ceil(sqrt(n))`, `rows = ceil(n / cols)`; `gap = (GRID_TILE_GAP * scale).round()`; граница колонки `c`: `left = body.x + ((c as f32) * (body.w + gap) / cols as f32).round()`, `right = body.x + (((c + 1) as f32) * (body.w + gap) / cols as f32).round() - gap`; ряды — так же по `body.h`; плитка `i` — колонка `i % cols`, ряд `i / cols`, поэтому последний неполный ряд выровнен влево с той же шириной. Заголовок — верхние `(GRID_TILE_HEADER * scale).round()` пикселей плитки, тело — остаток. Внешних отступов нет.

- [ ] **Step 1: тесты (красные)**
  - `terminal_grid_size_matches_current_body_formula`: для окна 1280×800 при scale 1, 1.5, 2 и `char_w`/`char_h` из констант теста `cols`/`rows` совпадают с прежним выражением (`((body.w - 20*s)/char_w).floor().max(10)` и `terminal_visible_rows`); то же для тела каждой плитки `grid_tiles(.., n = 4, ..)` при scale 1, 1.5, 2 (§6 «для плиток — по их прямоугольнику»).
  - `grid_tiles_cover_body_without_overlap` для n = 1…9 при scale 1, 1.25, 1.5, 2: все границы целые (`x.fract() == 0.0` и т. д.), плитки внутри `body`, попарно не пересекаются, соседи в ряду разделены ровно `gap`, порядок слева направо и сверху вниз; n = 3: две плитки в первом ряду, третья под первой той же ширины; n = 1: одна плитка на всё тело.
  - `grid_fits_uses_raw_size_before_clamps`: подобрать тело так, что `raw_cols = 9.5` — `false`, хотя `cols` после зажима 10; `raw_rows = 1.9` — `false`; ровно 10 × 2 — `true`.
  - `panel_hit_selects_header_body_and_gap`: точка в заголовке — `Header(i)`, в теле — `Body(i)`, в промежутке — `None`.
- [ ] **Step 2:** `make test TEST_FILTER=panels::` и `make test TEST_FILTER=terminal_grid_size` — FAIL.
- [ ] **Step 3:** реализация; `render_terminal_body` переходит на `terminal_grid_size` без изменения результата.
- [ ] **Step 4:** те же команды — PASS; `make test TEST_FILTER=terminal_ui` — PASS.
Тесты этой задачи лежат в модуле `panels` (фильтр `panels::`) или начинаются с `terminal_grid_size`; фильтры `grid_mode_`, `grid_pointer_`, `grid_frame_` зарезервированы за Tasks 4–6.
- [ ] **Step 5:** `make api-map`; коммит `src/panels.rs src/main.rs src/renderer/terminal_ui.rs PROJECT_AI_MAP.txt`.

### Task 3: Рендер по списку панелей

**Files:**
- Modify: `src/renderer/terminal_ui.rs` (`render_terminal_app` :3395, `render_terminal_body` :3468, `draw_terminal_tabs` :2339, `draw_fps_overlay` вызов :3463)
- Modify: `src/renderer.rs` (поля кэша рядом с `terminal_tab_display_titles` :511)
- Modify: `src/runtime.rs` (`TerminalRenderParams`, `render_terminal_and_present` :2980, аксессор размеров ячейки)
- Modify: `src/app.rs` (поля App :486-525 и их инициализация, вызов рендера и разбор результата :2243-2285, `on_resize_with_logical_size` :1822, `on_scale_changed` :1844, смена шрифта :1449-1455 и `FontReset` :1466-1468, создание runtime; тест на структуру :2862-2938 — только если проверяемый им текст меняется)
- Modify: `src/search.rs` (`terminal_search_geometry` :104-118 и его тесты :938, :995, :1017), `src/input.rs` (вызов :3441)
- Modify: `src/app/headless_run.rs` (тестовая обвязка, см. Interfaces)
- Test: `src/renderer/terminal_ui.rs`, `src/app.rs`, `src/search.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review рендер N тел: обычный режим неизменен, поиск и курсор только у активной панели, без аллокаций в кадре

**Interfaces:**
- Consumes: `Panel`, `terminal_grid_size`, `terminal_body_rect` (Task 2).
- Produces:
  - `TerminalRenderParams` получает `panels: &[Panel]`, `panel_layouts: &mut Vec<TerminalUiLayout>`, `tab_strip_hint: Option<&'static str>`; `active_terminal` остаётся (курсор, поиск, полоса вкладок);
  - `render_terminal_app` возвращает раскладку панели `active_terminal` (или `Default`), а `panel_layouts` заполняет раскладками всех панелей в порядке `panels` (`clear()` в начале);
  - `struct GridMetrics { width: f32, height: f32, scale: f32, char_w: f32, char_h: f32 }` — поле App `grid_metrics`, обновляет `fn refresh_grid_metrics(&mut self)` из runtime (новый `WindowRuntime::terminal_cell_size(&self) -> (f32, f32)` через новый `pub(crate)` аксессор в `terminal_ui.rs` над приватной `terminal_char_width` :2009 и `line_height * TERMINAL_TEXT_SCALE`; размер окна и scale — из renderer) после создания runtime, в `on_resize_with_logical_size`, `on_scale_changed`, при смене шрифта и `FontReset`; без runtime поле не меняется. `GridMetrics` — `#[derive(Clone, Copy, PartialEq, Default)]`; запись поля — только через `fn set_grid_metrics(&mut self, metrics: GridMetrics)` (её вызывает `refresh_grid_metrics`, тесты зовут её напрямую; Task 4 добавляет в неё рост поколения);
  - поля App `panels: Vec<Panel>`, `panel_layouts: Vec<TerminalUiLayout>`, `fn rebuild_panels(&mut self)` — в этой задаче только обычный режим: одна панель `active_terminal`, `body = terminal_body_rect(grid_metrics.width, grid_metrics.height, grid_metrics.scale)`, `header: None`, `content_hidden: false` (Task 4 добавляет сетку, Task 6 — `content_hidden`); `on_frame` вызывает `rebuild_panels()` перед рендером;
  - `fn ellipsize_to_width(text: &str, max_w: f32, advance: impl FnMut(char) -> f32, out: &mut String)` — чистая функция обрезки с «…»;
  - тестовая обвязка App с runtime для Tasks 3, 5, 6: `#[cfg(test)] pub(crate) mod test_support` в `src/app/headless_run.rs` с `pub(crate) fn started_headless_with_test_terminals(tag: &str, count: usize) -> (App, Receiver<String>, Profile, PathBuf)` — поверх существующего `started_headless` (`src/app/headless_run.rs:253`, сейчас приватный в `mod tests`; перенести в `test_support` вместе с `workspace_root` :165, `mod tests` переходит на них): после старта заменяет `app.terminals` на `count` штук `Terminal::new_for_test` с `grid.lock().unwrap().mark_presentation_ready()` (как `src/app.rs:2739`), `active_terminal = 0`, `active_terminal_presented = true`. Причина: у живых терминалов `started_headless` поток заголовков каждые 250 мс перезаписал бы флаг из `set_claude_foreground_for_test`, а `test_input` у них `None` (`src/terminal.rs:1814`, перехват только при `Some`, :2218) — байты PTY в тесте не видны. Если рендер или `on_frame` с подменёнными терминалами не работает — NEEDS_CONTEXT с причиной, не обходить.

Решения (§4.1 «Рендер», §4.3 «Заголовок плитки»):
- Кадр очищается один раз в начале `render_terminal_app` (перенос из тела :3478-3480 и пустой ветки :3430-3434).
- `render_terminal_body(terminal, body: Rect, search: Option<&mut TerminalSearchState>, show_cursor: bool, tile: bool, window_focused, pointer_x, pointer_y)`: все величины тела от `body` аргумента; `prepare_terminal_grid_for_render` получает `Option` и обновляет поиск только при `Some`; подсветка совпадений и `draw_search_overlay` — только при `Some`; оверлей поиска — в прямоугольнике тела (§4.3): `terminal_search_geometry` получает `body: Rect` вместо ширины окна и считает `x` и ширину от `body.x`/`body.w` (сейчас — от `self.width`, `src/renderer/terminal_ui.rs:2078`, прижат к правому краю окна); в обычном режиме `body` — тело окна, результат прежний, тесты `src/search.rs` переходят на новую сигнатуру без смены ожиданий; курсор — при `show_cursor` (= панель `active_terminal` и `focused && settings_progress <= 0.0`); акцентный разделитель :3744-3753 — только при `!tile`. Тело рисуется под scissor своего прямоугольника.
- Плитка (`header: Some`): заголовок — `write_display_title` терминала, обрезка `ellipsize_to_width` по ширине заголовка, результат кэшируется в векторе Renderer по индексу плитки: пересчёт только при смене исходного текста или ширины (сравнение с сохранённой копией в том же элементе, без новой аллокации при совпадении). Замыкание `advance` берёт `&mut self` для метрик глифов, поэтому кэш на время обрезки вынимается `std::mem::take` (как `terminal_tab_widths` в `draw_terminal_tabs`). Плитка `active_terminal` — рамка акцентным цветом толщиной `scale.round().max(1.0)` вокруг заголовка и тела.
- Панель с `content_hidden` рисует только фон своего тела (флаг выставляет Task 6; здесь он всегда `false`).
- FPS-оверлей — от `terminal_body_rect` окна.
- `tab_strip_hint: Some(text)` — текст справа на полосе вкладок цветом заголовка неактивной вкладки поверх фона полосы.
- В App после рендера: `interaction.update_layout(active)`, clamp `scroll_y` активного как сейчас; `panel_layouts` хранится до следующего кадра.

- [ ] **Step 1: тесты (красные)**
  - `ellipsize_to_width_keeps_fitting_text_and_cuts_with_ellipsis`: advance 1.0 на символ; «abcdef» при 10 → «abcdef»; при 4 → «abc…»; при 0.5 → пусто.
  - `panels_render_two_bodies_with_search_only_on_active` (в `src/app.rs`, обвязка `started_headless_with_test_terminals(.., 2)`): две панели (тела — левая и правая половина тела окна, `app.panels` задаётся в тесте после `rebuild_panels`), поиск открыт у активной; после `on_frame` `panel_layouts.len() == 2`, `panel_layouts[i].body` равны телам панелей, у активной `layout.search` лежит внутри её `body`, у неактивной `layout.search == None`, для неактивного терминала refresh поиска не вызывался (сетка неактивного изменена после открытия поиска — его кэш совпадений не пересчитан).
  - `terminal_search_geometry_stays_inside_body`: тело — левая половина окна; прямоугольник оверлея внутри `body`; для тела окна — прежний результат.
  - `panels_normal_mode_layout_matches_window_body`: одна панель обычного режима — возвращённая раскладка совпадает с раскладкой до правки (`body == terminal_body_rect(..)`, те же `cols`/`visible_rows`).
  - Тест на структуру `src/app.rs:2862-2938` обновить, только если проверяемые им строки рендера изменились, сохранив инварианты (порядок `mark_presentation_layout_ready` перед `presentation_visible`).
- [ ] **Step 2:** `make test TEST_FILTER=ellipsize`, `make test TEST_FILTER=panels_` и `make test TEST_FILTER=terminal_search_geometry` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** `make test TEST_FILTER=ellipsize`, `make test TEST_FILTER=panels_`, `make test TEST_FILTER=search`, `make test TEST_FILTER=terminal_ui`, `make test TEST_FILTER=headless` — PASS (headless-снимки обычного режима — регрессия «вне сетки не меняется»).
- [ ] **Step 5:** `make api-map`; коммит изменённых файлов и `PROJECT_AI_MAP.txt`.

### Task 4: Режим сетки в App: вход, выход, состав, фокус, видимость

**Files:**
- Modify: `src/app.rs` (поля App :470-527 и их инициализация, `GlobalShortcut` :73-79, `global_shortcut` :136-155, `handle_global_key` :1727-1745, `add_terminal_with_launch` :760, `select_terminal_tab_from_user` :744, `close_terminal_tab_at` :959-1015, `drain_terminal_host_events` :2313-2338, расчёт дедлайна цикла :1659-1672, `on_about_to_wait`, `rebuild_panels` и `set_grid_metrics` из Task 3, разбор результата кадра :2276)
- Modify: `src/app/tab_delivery.rs` (`handle_tab_status` :104-119)
- Test: `src/app.rs`, `src/app/tab_delivery.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review жизненный цикл сетки: пути выхода, перенос фокуса без выхода, отложенные намерения активации, 1004

**Interfaces:**
- Consumes: `Terminal::claude_foreground`, `set_claude_foreground_for_test` (Task 1); `grid_tiles`, `grid_fits`, `Panel` (Task 2); `panels`, `panel_layouts`, `rebuild_panels`, `grid_metrics`, `set_grid_metrics`, `refresh_grid_metrics`, `tab_strip_hint` (Task 3).
- Produces:
  - `GlobalShortcut::ToggleClaudeGrid` — `KeyCode::KeyS` при `exact_global_modifiers(modifiers, true)` (как Ctrl+Shift+T); гейты диалога закрытия и настроек уже стоят на этом пути (`src/app.rs:1884`, `:1899`);
  - поля App `claude_grid: Option<ClaudeGrid>` (`struct ClaudeGrid { members: Vec<usize> }` — индексы вкладок-плиток в порядке вкладок), `grid_generation: u64`, `grid_hint: Option<(GridHint, Instant)>`; scratch-поля `grid_members_scratch: Vec<usize>` (пересчёт состава) и `grid_tile_rects: Vec<(Rect, Rect)>` (выход `grid_tiles` в `rebuild_panels`) — `clear()` и повторное использование;
  - `pub(crate) fn grid_tile_count(&self) -> usize` (0 при закрытой сетке; нужен Task 7 — `claude_grid` приватно);
  - все поля App, нужные Tasks 5 и 6 (заводятся и инициализируются здесь, чтобы 5 и 6 не правили `struct App` и конструктор): `panel_layouts_generation: u64` — присваивается `= grid_generation` в разборе результата кадра (:2276) этой задачей; `suppressed_release: Option<PointerButton>`, `last_header_press: Option<(Instant, usize)>`, `hovered_panel_terminal: Option<usize>` (Task 5); `withheld_until: Option<Instant>` (Task 6);
  - `enum GridHint { NoClaudeTabs, TooManyTabs }`, `fn text(self) -> &'static str` (тексты из Global Constraints), `const GRID_HINT_DURATION: Duration = Duration::from_secs(2)`;
  - `pub(crate) fn grid_open(&self) -> bool`, `fn tab_visible(&self, index: usize) -> bool`, `fn toggle_claude_grid(&mut self, now: Instant)`, `fn exit_claude_grid(&mut self, target: Option<usize>)`, `fn sync_claude_grid(&mut self, now: Instant)`, `fn advance_grid_hint(&mut self, now: Instant)`, `fn bump_grid_generation(&mut self)` — единственное место роста поколения; увеличивает `grid_generation` и снимает захват указателя существующим `interaction.cancel_pointer_interaction(&mut terminals[active_terminal])` (`src/input.rs:250`, при наличии активного терминала).

Решения (§2 Q2, Q5, Q6, Q8, Q9; §4.1 «Видимость»; §4.3):
- Состав — `(0..terminals.len()).filter(|i| terminals[i].claude_foreground())`. `sync_claude_grid` вызывается в `on_about_to_wait` и после `close_terminal_tab_at`; при закрытой сетке выходит сразу (без обхода терминалов и блокировок `title_cache`). При открытой пересчитывает состав в `grid_members_scratch`; при отличии от `members` (набор или порядок) — `bump_grid_generation()`, затем: пусто → `exit_claude_grid(None)` (остаётся текущая `active_terminal`); у `active_terminal` пропал флаг → фокус на следующую по порядку плитку после её позиции в старом составе, иначе на предыдущую (`cancel_terminal_presentation_intents()`, `activate_ready_terminal(i, false)`). Независимо от смены состава `!grid_fits(..)` по текущим `grid_metrics` → `exit_claude_grid(Some(active))` и `grid_hint = TooManyTabs`. `request_frame` — только если что-то из этого изменилось.
- Метрики (P2-1): `set_grid_metrics` при открытой сетке и `metrics != grid_metrics` вызывает `bump_grid_generation()` — сразу в обработчике resize, scale или шрифта. Причина: в одном шаге цикла configure и scale обрабатываются до событий ввода, а `on_about_to_wait` — после них (`src/app/direct_wayland.rs:132-179`, :266); без этого нажатие из того же пакета сопоставлялось бы с раскладкой прошлого кадра при совпадающем поколении. Q9 по новым метрикам проверяет `sync_claude_grid` в `on_about_to_wait` того же шага.
- Вход (Ctrl+Shift+S при закрытой сетке; проверить, что путь глобальных хоткеев не срабатывает при открытых настройках и диалоге закрытия, иначе добавить гейт): состав пуст → `grid_hint = NoClaudeTabs`, сетка не открывается; `!grid_fits` → `TooManyTabs`; иначе `cancel_terminal_presentation_intents()`, `claude_grid = Some`, рост поколения, фокус — `active_terminal`, если он в составе, иначе первая плитка через `activate_ready_terminal(first, false)`.
- Вход и выход хоткеем — `toggle_claude_grid(Instant::now())` из `handle_global_key`; `now` параметром, чтобы тест проверял дедлайн подсказки точно.
- Выход: повторный Ctrl+Shift+S → `exit_claude_grid(Some(active_terminal))`; `select_terminal_tab_from_user` и `add_terminal_with_launch` в начале закрывают сетку (`claude_grid = None`, рост поколения), дальше их обычный путь; в `add_terminal_with_launch` — до строки `self.runtime.as_ref()?` (:761), чтобы выход происходил в момент запроса и тестировался без runtime. `exit_claude_grid(Some(t))`: `claude_grid = None`, рост поколения, при `t != active_terminal` — `cancel_terminal_presentation_intents()` и `activate_ready_terminal(t, false)`. Двойной клик по заголовку — Task 5.
- `close_terminal_tab_at` в сетке при `index == old_active`: цель — следующая плитка после `index` в составе (с учётом сдвига индексов после удаления), иначе предыдущая; есть цель — `self.active_terminal = цель` (как сейчас присваивается результат `active_index_after_remove`, :1007), затем `activate_ready_terminal(цель, false)`, без `ActivateWhenReady` соседу; цели нет — обычный путь и выход из сетки. Закрытие другой вкладки — как сейчас, затем `sync_claude_grid`.
- `rebuild_panels` в сетке: панели из `members` и `grid_tiles(terminal_body_rect(..), n, scale, ..)`, `header: Some`.
- `tab_visible(i)`: сетка открыта — `members.contains(&i)`; иначе `i == active_terminal && active_terminal_presented`. `handle_tab_status` — `active: self.tab_visible(index)`. BEL: сетка открыта и `members.contains(&index)` — без отметки; иначе `index != active`; внутри `iter_mut` по `terminals` метод `tab_visible` не вызвать (заимствование), поэтому `members` берётся до цикла (`self.claude_grid.as_ref()`, поле отдельно от `terminals`).
- Подсказка: `tab_strip_hint = grid_hint` с непрошедшим дедлайном; дедлайн подсказки — в расчёт `WaitUntil` в обоих режимах. Снимает её `advance_grid_hint(now)` — вызов в `on_about_to_wait` независимо от того, открыта ли сетка (`NoClaudeTabs` показывается именно при закрытой): дедлайн прошёл → `grid_hint = None` и один `request_frame`; иначе ничего.

- [ ] **Step 1: тесты (красные), pure App (`App::new()`) с `Terminal::new_for_test`, у каждой вкладки `mark_presentation_ready()` (как в `src/app.rs:2740`), `active_terminal_presented = true`, `set_claude_foreground_for_test`, `grid_metrics` задан в тесте. Имена — с префиксом `grid_mode_`.**
  - `grid_mode_toggle_enters_with_claude_tabs_and_exits_to_focused_tile`: вкладки [claude, shell, claude], активна 1 → вход: `members == [0, 2]`, активна 0; повтор — сетка закрыта, активна 0.
  - `grid_mode_toggle_without_claude_tabs_shows_hint_with_deadline`: `toggle_claude_grid(now)` → `grid_open() == false`, `grid_hint == Some((NoClaudeTabs, now + 2 s))`; `advance_grid_hint(now + 1 s)` — подсказка на месте; `advance_grid_hint(now + 2 s)` — снята, кадр запрошен; повторный вызов кадра не запрашивает.
  - `grid_mode_toggle_ignored_with_settings_or_close_dialog_open` (через путь клавиши, который проходит гейты :1884, :1899).
  - `grid_mode_exit_paths`: клик по вкладке полосы (`select_terminal_tab_from_user(1)`) — выход, активна 1; `add_terminal_with_launch` без runtime — сетка закрыта, хотя вкладка не создана (`None`).
  - `grid_mode_focus_moves_without_exit_when_focused_tile_loses_claude`: [c, c, c], фокус 1, флаг 1 снят → `sync_claude_grid`: активна 2, сетка открыта, байты 1004: у 1 `\x1b[O`, у 2 `\x1b[I` (`enable_focus_reporting_for_test`); последняя плитка — предыдущая; все флаги сняты → выход, активна прежняя.
  - `grid_mode_close_focused_tab_moves_to_next_tile_not_shell`: [c, shell, c], фокус 0, закрыть 0 → активна бывшая 2 (новый индекс 1), сетка открыта, ни у одной вкладки нет `ActivateWhenReady`, у оболочки нет байтов 1004.
  - `grid_mode_pending_activation_is_cancelled_on_entry`: висящее `ActivateWhenReady` у вкладки 1 → вход → `process_terminal_presentation_intents` фокус не меняет.
  - `grid_mode_q9_blocks_entry_and_exits_open_grid`: метрики с маленьким окном → вход не открывает, `TooManyTabs`; открытая сетка, уменьшить метрики через `set_grid_metrics` (и отдельно — увеличить `char_w`, смена шрифта) → `sync_claude_grid` выходит на плитку в фокусе с подсказкой.
  - `grid_mode_generation_grows_on_composition_and_metrics_change`: смена состава в `sync_claude_grid` и `set_grid_metrics` с другими метриками при открытой сетке увеличивают `grid_generation` сразу (до `sync_claude_grid`) и снимают захват указателя (видно через `pressed_mouse_buttons_for_test`, `src/input.rs:428`); `set_grid_metrics` с теми же метриками и при закрытой сетке поколение не меняет.
  - `grid_mode_sync_does_nothing_when_closed`: сетка закрыта, флаги стоят — `sync_claude_grid` не меняет поколение, не запрашивает кадр.
  - `grid_mode_tab_status_reports_active_for_every_tile` (в `tab_delivery.rs`) и `grid_mode_bell_not_marked_on_visible_tiles`; вне сетки — существующие тесты без изменений.
- [ ] **Step 2:** `make test TEST_FILTER=grid_mode_` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** `make test TEST_FILTER=grid_mode_`, `make test TEST_FILTER=tab_status`, `make test TEST_FILTER=bell` — PASS.
- [ ] **Step 5:** `make api-map`; коммит.

### Task 5: Указатель в сетке

**Files:**
- Modify: `src/input.rs` (`mouse_wheel` :939, `update_link_hover` :193, новый метод снятия захвата; `ClickCounter::WINDOW` :1085)
- Modify: `src/app.rs` (`on_pointer_button` :2051-2150, `on_pointer_motion` :1955-2017, `on_scroll` :2159-2193, `update_link_hover` :1591; тест на структуру :4653-4687)
- Test: `src/app.rs`, `src/input.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review маршрутизация мыши по панелям, поколение раскладок, непарные release

**Interfaces:**
- Consumes: `panels`, `panel_layouts`, `rebuild_panels` (Task 3); `grid_open`, `grid_generation`, `panel_layouts_generation`, `suppressed_release`, `last_header_press`, `hovered_panel_terminal`, `exit_claude_grid`, `bump_grid_generation`, `set_grid_metrics` (Task 4; поля App уже заведены там — `struct App` и конструктор здесь не меняются); `panel_hit`, `PanelHit` (Task 2); `started_headless_with_test_terminals` (Task 3).
- Produces: `TerminalInteraction::mouse_wheel(&mut self, delta: ScrollDelta, terminal: &mut Terminal, layout: TerminalUiLayout) -> bool`; `update_link_hover(&self, terminal: &mut Terminal, x: f32, y: f32, layout: TerminalUiLayout) -> bool` (вызовы в тестах `src/input.rs` :2647, :2661, :2671, :2674, :2751 и вызовы `mouse_wheel` обновить); `pub(crate) const MULTI_CLICK_WINDOW: Duration` (на него переходит `ClickCounter::WINDOW`); `#[cfg(test)] TerminalInteraction::set_pointer_for_test(&mut self, x: f32, y: f32)` (`mouse_x`/`mouse_y` приватны, :21).

Решения (§4.1 «Ввод», «Поколение состава»; Q2):
- В сетке `on_pointer_button`, `on_pointer_motion` и `on_scroll` в начале вызывают `rebuild_panels()` (§4.1: список пересобирается перед событиями указателя), hit-test — по свежему списку. `rebuild_panels` не читает `/proc` (сканирование баннера — только перед рендером, Task 6).
- Ветка сетки — в `on_pointer_button` после веток диалога, настроек и полосы вкладок, перед маршрутизацией в терминал; вне сетки код пути не меняется, кроме проверки `suppressed_release` ниже (вне сетки она `None`, пока её не поставила сетка).
- Любое нажатие в сетке, которое не дошло до `mouse_input` (заголовок, промежуток, устаревшее поколение), ставит `suppressed_release = Some(button)`; парное отпускание поглощается и в терминал не уходит. Проверка «`suppressed_release == Some(button)` → `take()` и поглотить» (отпускание другой кнопки отметку не снимает) стоит в начале обработки отпускания независимо от `grid_open()`: после выхода двойным кликом по заголовку отпускание приходит уже вне сетки, а `mouse_input` шлёт release без проверки парного нажатия (`src/input.rs:915-926`).
- Поколение проверяется только в сетке (§6): вне сетки несовпадение `panel_layouts_generation` с `grid_generation` на маршрутизацию не влияет.
- Несовпадение `panel_layouts_generation != grid_generation` (только в сетке): нажатие — только выбор плитки под точкой (если другая) и `request_frame`; колесо и hover пропускаются.
- `Header(i)`: левое нажатие; повторное по той же плитке в пределах `MULTI_CLICK_WINDOW` → `exit_claude_grid(Some(panels[i].terminal))`; иначе запомнить в `last_header_press` и выбрать плитку.
- `Body(i)` с `panels[i].terminal != active_terminal`: `cancel_terminal_presentation_intents()`, `activate_ready_terminal(t, false)`, `interaction.layout = panel_layouts[i]`, затем существующий `mouse_input` (средняя кнопка вставляет в эту плитку).
- Захват снимает `bump_grid_generation` (Task 4); иначе он остаётся у активной панели до отпускания, координаты не зажимаются.
- Колесо: панель под указателем, `mouse_wheel(delta, &mut terminals[t], panel_layouts[i])`, без смены фокуса; вне сетки — `interaction.layout`.
- Hover: в сетке — только панель под указателем с её раскладкой; при смене панели или уходе за панели `set_hovered_link(None)` у прежней (`hovered_panel_terminal`).

- [ ] **Step 1: тесты (красные) на App с runtime — `started_headless_with_test_terminals` (Task 3; `mouse_input` вызывается только при runtime, `src/app.rs:2138-2145`; у тестовых терминалов есть `test_input` и нет потока заголовков): флаги через `set_claude_foreground_for_test`, вход в сетку, один кадр (`on_frame`), затем события через `on_pointer_motion`/`on_pointer_button`/`on_scroll`; байты PTY — `input_bytes_for_test`. Имена — с префиксом `grid_pointer_`.**
  - `grid_pointer_click_in_inactive_tile_focuses_it_and_routes_press`: активна 0, нажатие в теле плитки 1 → активна 1, 1004 out/in, `interaction.layout == panel_layouts[1]`, сетка открыта.
  - `grid_pointer_click_cancels_pending_activation` (§6): висящее `ActivateWhenReady` у вкладки без Claude → клик по плитке → `process_terminal_presentation_intents` фокус не перехватывает.
  - `grid_pointer_wheel_over_inactive_tile_scrolls_it_without_focus`: у терминала плитки 1 заранее есть scrollback (через `feed_batch` строк больше высоты; колесо зажимает цель по `max_scroll`, `src/input.rs:976`) → цель `scroll_y` изменилась, активна 0.
  - `grid_pointer_header_double_click_exits_to_tile`: два нажатия в пределах `MULTI_CLICK_WINDOW` — выход, активна вкладка плитки; у неё включён mouse tracking, второе отпускание приходит уже вне сетки — в PTY ничего не ушло; одно нажатие — не выход.
  - `grid_pointer_resize_then_press_before_frame_not_routed` (P2-1): после кадра `on_resize_with_logical_size` с другим размером, сразу нажатие в теле плитки без нового кадра — только выбор плитки, в PTY с mouse tracking ничего.
  - `grid_pointer_generation_ignored_outside_grid`: сетка закрыта, `panel_layouts_generation != grid_generation` — нажатие с mouse tracking уходит в PTY как раньше.
  - `grid_pointer_stale_generation_press_only_selects_and_swallows_release`: после `bump_grid_generation` без нового кадра нажатие выбирает плитку, при включённом mouse tracking (`\x1b[?1000h` через `feed_batch`) в PTY не уходит ни нажатие, ни отпускание; колесо — без эффекта.
  - `grid_pointer_header_press_release_in_body_not_sent`: нажатие в заголовке, отпускание над телом активной плитки с mouse tracking — в PTY ничего.
  - `grid_pointer_hover_moves_between_tiles_and_clears_previous`.
  - Вне сетки: существующие тесты мыши зелёные; `left_press_clear_and_cursor_leave_are_routed_before_ui_short_circuits` обновить, только если проверяемый текст изменился, сохранив порядок проверок.
- [ ] **Step 2:** `make test TEST_FILTER=grid_pointer_` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** `make test TEST_FILTER=grid_pointer_`, `make test TEST_FILTER=input` — PASS.
- [ ] **Step 5:** `make api-map`; коммит.

### Task 6: Кадры, анимации и баннер dev-channels в сетке

**Files:**
- Modify: `src/app.rs` (`advance_dev_channel_warnings` :800-813, `active_dev_warning_hidden` :815, `withhold_active_frame` :826-842, поле `frame_withheld` :520 и комментарии, `sync_animation_state` :1616-1631, обновление прокрутки в `on_frame` :2224-2234 и clamp после кадра :2279-2280, расчёт дедлайна :1659-1672; `rebuild_panels` не меняется — `content_hidden` выставляется в `on_frame`)
- Modify: `src/input.rs` (если условие анимации прокрутки терминала нужно вынести из `animation_active`)
- Test: `src/app.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review простой сетки и дедлайны скрытия баннера: нет бесконечного будения, нет пропущенной перерисовки

**Interfaces:**
- Consumes: `panels`, `panel_layouts`, `rebuild_panels`, `started_headless_with_test_terminals` (Task 3), `grid_open`, `grid_generation`, `panel_layouts_generation`, поле `withheld_until` (Task 4).
- Produces: `Panel::content_hidden` выставляется в `on_frame` перед рендером (не в `rebuild_panels`: тот вызывается и на событиях указателя, а сканирование читает `/proc`, AGENTS §3).

Решения (§4.1 «Кадры и анимации», «Баннер dev-channels»):
- `on_frame`: `scroll_y.update(dt)` для терминала каждой панели; clamp неактивных панелей до рендера — по `max_scroll` раскладки своей панели из `panel_layouts`, и только при `panel_layouts_generation == grid_generation` (после смены состава индекс в `panel_layouts` прошлого кадра указывает на другую панель; тогда clamp неактивных — после рендера по свежим раскладкам); для активной — как сейчас по `interaction.layout`; автопрокрутка выделения — только активная.
- `sync_animation_state`: анимация терминала — активная по `interaction.animation_active` как сейчас, плюс любая другая видимая панель с неуспокоенной плавной прокруткой (то же условие прокрутки, что `animation_active` проверяет для терминала).
- Вне сетки баннер — без изменений (`withhold_active_frame`, `frame_withheld`).
- В сетке перед кадром для терминала каждой панели — `observe_dev_channel_warning` и `observe_dev_channel_banner` (как сейчас в `withhold_active_frame`), `content_hidden = dev_warning_frame_hidden(now)`; кадр не удерживается; панель с `content_hidden` рисует только фон тела (это уже делает рендер Task 3).
- После кадра в сетке: `withheld_until` = минимум `dev_warning_hidden_until` среди панелей с `content_hidden`, иначе `None`. `advance_dev_channel_warnings` в сетке: `withheld_until.is_some_and(|t| now >= t)` → `None` и `request_frame`. Дедлайн цикла в сетке — `withheld_until` вместо дедлайна активной.
- Комментарии к `frame_withheld` и `advance_dev_channel_warnings` описывают оба режима.

- [ ] **Step 1: тесты (красные) на App с runtime — `started_headless_with_test_terminals` (Task 3; `on_frame` без runtime сразу возвращает, `src/app.rs:2203`, `:612`). Имена — с префиксом `grid_frame_`.**
  - `grid_frame_scroll_animation_of_inactive_tile_keeps_frames`: у терминала плитки 1 есть scrollback (`feed_batch` строк больше высоты), цель прокрутки отличается от текущей → `animation_active`; цикл `on_frame` до успокоения, не больше 200 кадров — успокоилась, `animation_active == false`. В тестах `grid_frame_*` перед каждым `on_frame` ставить `app.last_frame = Instant::now() - Duration::from_millis(16)`: `dt` считается по реальному времени (`src/app.rs:2212`) и только ограничен сверху 16 мс (`:95-97`), поэтому без этого в плотном цикле `dt` — 1–2 мс и 200 кадров анимацию не успокоят; с ним 200 кадров — 3,2 с модельного времени.
  - `grid_frame_clamp_skipped_for_stale_layouts`: постановка — `max_scroll` раскладки прошлого кадра под индексом неактивной плитки меньше её цели прокрутки, а `max_scroll` свежей раскладки не меньше цели; после смены состава (`bump_grid_generation`) первый `on_frame` цель не урезает.
  - `grid_frame_idle_settles_without_frames`: вход в сетку, кадры до успокоения, затем `loop_control` — `Wait` или дедлайн не раньше реального события.
  - `grid_frame_dev_warning_hides_only_its_tile_and_redraws_after_deadline`: две плитки, у 1 предупреждение (тот же способ, что `dev_warning_withholds_only_the_active_tab_until_it_clears_or_times_out`, :2500-2514); кадр `on_frame` → `content_hidden` только у 1, кадр не удержан; `withheld_until == dev_warning_hidden_until` плитки 1; `advance_dev_channel_warnings(t)` в дедлайн — dirty, `withheld_until == None`; следующий вызов без вывода — без нового кадра.
  - `grid_frame_two_hidden_tiles_redraw_first_at_its_deadline`: две скрытые плитки с разными дедлайнами — в первый дедлайн кадр запрошен, новое `withheld_until` — дедлайн второй.
  - Существующий тест баннера вне сетки — без изменений.
- [ ] **Step 2:** `make test TEST_FILTER=grid_frame_` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** `make test TEST_FILTER=grid_frame_`, `make test TEST_FILTER=dev_warning` — PASS.
- [ ] **Step 5:** `make api-map`; коммит.

### Task 7: Headless-снимок сетки

**Files:**
- Modify/Test: `src/headless/tests_protocol.rs` (там приватная `png_size`, :88)

**Агенты:** implementer ds-low-agentic · reviewer нет

**Interfaces:**
- Consumes: `tab_input::test_support` (Task 1), `grid_tile_count` (Task 4), команды headless `key` и `screenshot` (`src/headless/protocol.rs:110`, `:126`), `session_for_test`, `run_script` (`src/headless/tests.rs:24`, `:63`); весь режим сетки (Tasks 1–6).

Решение (§6 Headless): сессия `session_with_shell(1280, 800, Some(claude_copy))` (`src/headless/tests.rs:29`) — `shell_override` задаёт оболочку каждой вкладки (`src/app.rs:496-497`, `:762`), поэтому подставной Claude (копия `/usr/bin/cat` под именем `claude` через `TestDir::binary_named`, эталон и прогрев против ETXTBSY — Task 1 Interfaces) сам лидер группы с `comm == "claude"` и не читает rc-файлов. Ещё три вкладки — `key ctrl+shift+t` (образец `src/headless/tests_misc.rs:147`); ожидание флагов — свой ограниченный цикл (не `wait_until`: его предикат получает JSON `dump`, где флага Claude нет, `src/headless/tests.rs:71-76`, `src/headless/dump.rs:151`; формат dump не трогать): пока `session.app_mut().terminals()` (`src/app/headless_run.rs:110`) не даёт `len() == 4 && all(claude_foreground)`, проверить дедлайн 3 с (истёк — тест падает) и `run_script(&mut session, b"wait 50\n")`; `key` с Ctrl+Shift+S; `screenshot` в `/tmp/ronsole-claude-grid-4.png`; проверки: ответ `ok`, размер PNG (`png_size`, `src/headless/tests_protocol.rs:88`) равен 1280×800, `session.app_mut().grid_open()` (`src/headless/session.rs:205`) — `true`, `grid_tile_count() == 4` (Task 4).

- [ ] **Step 1:** тест `headless_claude_grid_screenshot_four_tiles`.
- [ ] **Step 2:** `make test TEST_FILTER=headless_claude_grid` — PASS, файл `/tmp/ronsole-claude-grid-4.png` существует.
- [ ] **Step 3:** коммит.

---

## Порядок и параллель

Task 1 и Task 2 независимы. Task 3 — после Task 2. Task 4 — после Task 1 и Task 3 (общий `src/app.rs`: `rebuild_panels`, поля App). Task 5 — после Task 4; Task 6 — после Task 5 (обе правят `src/app.rs`, тесты и `on_frame`-окрестность; последовательно, чтобы не ловить конфликт слияния). Task 7 — последней. Финальный гейт — `make codex_test` (главная), затем финальное ревью ветки.
