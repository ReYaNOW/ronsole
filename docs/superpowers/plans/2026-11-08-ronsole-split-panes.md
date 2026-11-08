# Ronsole: сплиты панелей внутри вкладки — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** вкладку терминала можно делить на вложенные панели (справа/снизу), у каждой свой shell и процесс; фокус — кликом и `Alt+стрелками`, граница тянется мышью, закрытие панели отдаёт место соседней, закрытие последней закрывает вкладку.

**Architecture:** дерево `SplitNode` живёт поверх плоского хранилища `App.terminals`; `Tab` группирует листья одной вкладки и хранит `active_pane: PaneId`. `layout_panes` раз в кадр (в `rebuild_panels`) раскладывает дерево в прямоугольники панелей без аллокаций; рендер, мышь, сетка Claude и harness продолжают работать по уже существующему списку `panels`. Фокус меняется только через `activate_pane`.

**Tech Stack:** Rust (nightly через Makefile), glow/EGL, Wayland. Новых зависимостей нет.

**Spec:** `docs/superpowers/specs/2026-11-07-ronsole-split-panes-design.md` (утверждена 08.11). Номера §, Q и T ниже — из спеки; исполнитель читает разделы, на которые ссылается его задача.

## Global Constraints

- AGENTS.md §0.1: после правки любого `.rs` — `make api-map`, обновлённый `PROJECT_AI_MAP.txt` в том же коммите; `git diff --check`. Длинные сборки и прогоны — через `nohup`, опрос короткими командами.
- AGENTS.md §3: в кадре и в обработке вывода — без файлового I/O, `/proc`, `format!` и растущих аллокаций. `pane_rects`, `panels`, `panel_layouts`, `tab_labels` — поля долгоживущего состояния, `clear()` и повторное использование.
- AGENTS.md §3: все границы панелей, зазоров и рамок — целые пиксели; текст, курсор, выделение и hitbox панели — одна snapped-геометрия её `body`, без накопления дробных базлайнов.
- Терминальное поведение `crates/ronsole-term` не меняется; протокол harness (tab-status, `TAB_INPUT`, tab-wait) не меняется.
- Зазор между панелями — `(GRID_TILE_GAP * scale).round()`, `GRID_TILE_GAP = 6.0` (`src/panels.rs:9`). Рамка активной панели — 2 px внутрь, цвет акцента того же сепаратора фокуса.
- Минимум панели — `raw_cols >= 10` и `raw_rows >= 2` от `terminal_grid_size` (`src/renderer/terminal_ui.rs:1438`); выражается через `pane_min` (Task 1).
- Клавиши: `Ctrl+Shift+D` — сплит вправо, `Ctrl+Shift+E` — сплит вниз, `Ctrl+Shift+W` — закрыть панель, `Alt+←/→/↑/↓` — фокус, `Ctrl+4` — закрыть вкладку (не меняется).
- Текст подсказки невозможного сплита константный, ровно: «Панель слишком мала для сплита», 2 с через `set_grid_hint`.
- Поведение одиночной вкладки не меняется: существующие тесты остаются зелёными без правки ожиданий, кроме тестов на структуру исходника и фикстур, которые обязаны знать новую модель вкладки (Task 3).
- Узкий прогон — точными именами: `make test-one TEST='<mod>::tests::<name>'` (повторять для каждого теста задачи) или `make test TEST_FILTER=<подстрока>`; `TEST_FILTER` — подстрока, а не тег, поэтому в задачах названы точные имена. Полный `make codex_test` — один раз на ветку/PR.
- Раскладку между запусками не сохраняем (Q1).

## Review Focus

1. Окно уменьшили до предела при раскладке из 3 панелей: панели не перекрываются и неотрицательны, PTY получает минимум 10×2, а после восстановления окна пропорции прежние (Task 1, `layout_restores_ratios_after_shrink`).
2. Очистка процесса панели отказала во время закрытия: `close_pane` не меняет дерево, `terminals` и фокус; `close_tab` прекращает закрытие, уже удалённые панели остаются удалёнными, оставшиеся живы и фокус восстановлен по §6.4 (Task 4, `close_pane_refusal_keeps_tree_and_process`, `close_tab_refusal_stops_and_keeps_remaining_panes`).
3. `Ctrl+Shift+D` в панели, где сплит нарушит минимум: сплит не создаётся, процесс не спавнится, видна подсказка (Task 5, `split_in_narrow_pane_shows_hint_and_spawns_nothing`).
4. Клик по правой нижней панели при дробном scale: нажатие уходит в неё (координаты глобальные, `text_x` уже содержит `body.x`), парный release не теряется; клик в зазоре фокус не меняет (Task 7, `click_on_second_pane_focuses_it_and_delivers_press`, `click_in_gap_starts_divider_drag_and_changes_no_focus`).
5. Две панели одной вкладки: обе отвечают `tab-status active=1` при представленной вкладке, `TAB_INPUT` приходит ровно своей панели, а `Alt+стрелки` в списке из ≥2 панелей не уходят в shell (Tasks 8, 9).

---

### Task 1: Чистая геометрия раскладки панелей

**Files:**
- Create: `src/split.rs`
- Modify: `src/main.rs` (`mod split;` рядом с `mod panels;` :11)
- Test: `#[cfg(test)] mod tests` в `src/split.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review целочисленная геометрия раскладки при дробном scale и совпадение порога `pane_min` с `terminal_grid_size`

**Interfaces:**
- Produces:
  - `pub(crate) enum SplitAxis { SideBySide, Stacked }` — `SideBySide` = вертикальная граница (соседи по x), `Stacked` = горизонтальная. `Copy, Clone, PartialEq, Eq, Debug`.
  - `pub(crate) struct PaneId(u64)` — `Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug`; `PaneId::new(u64) -> Self`, `PaneId::raw(self) -> u64`.
  - `pub(crate) enum SplitNode { Leaf(PaneId), Split { axis: SplitAxis, ratio: f32, first: Box<SplitNode>, second: Box<SplitNode> } }` — `Clone, Debug`.
  - `pub(crate) fn pane_min(char_w: f32, char_h: f32, scale: f32) -> (f32, f32)` — `(10*char_w + 20*scale, 2*char_h + 16*scale)`.
  - `pub(crate) fn min_extent(node: &SplitNode, axis: SplitAxis, char_w: f32, char_h: f32, scale: f32) -> f32` — минимальный размер поддерева по этой оси, уже с `ceil` у ветвей.
  - `pub(crate) fn split_child_rects(node: Rect, axis: SplitAxis, ratio: f32, gap: f32, first_min: f32, second_min: f32) -> (Rect, Rect)` — единственная формула разреза (её же используют `layout_panes`, `divider_at` Task 2 и drag Task 7).
  - `pub(crate) fn layout_panes(root: &SplitNode, body: Rect, scale: f32, char_w: f32, char_h: f32, out: &mut Vec<(PaneId, Rect)>)`.
- Consumes: `crate::search::Rect` (`src/search.rs:11`, поля `x, y, w, h`); `crate::panels::GRID_TILE_GAP`.
- Потребители: Tasks 2–10.

Решения (§5):
- `gap = (GRID_TILE_GAP * scale).round()`; если размер узла по оси меньше `gap`, зазор узла `0`. `available = max(extent - gap, 0.0)`.
- Кламп: `lo = ceil(min_first) / available`, `hi = 1.0 - ceil(min_second) / available`; диапазон применяется, только если `lo <= hi`, иначе берётся сохранённый `ratio` (окно сильно уменьшили — делим по пропорциям).
- `first_extent = (available * clamped_ratio).floor()`, `second_extent = available - first_extent`; обе целые, вместе с `gap` точно покрывают родителя.
- `min_extent` для `Split` по оси разреза: `ceil(min_first_по_оси) + gap + ceil(min_second_по_оси)`; поперёк — `max(...)`. Лист — `pane_min` по оси, `ceil`.
- `layout_panes` делает `out.clear()`, обход pre-order first→second; внутри выделений нет. `ratio` в дереве не меняется — кламп только при отрисовке.
- Модуль — только чистые типы и геометрия: без `App`, `Terminal`, без рендера.

- [ ] **Step 1: тесты (красные)** в `src/split.rs`:
  - `layout_covers_body_without_overlap`: деревья на 1, 2, 3, 4, 6 листьев (включая вложенные смешанные оси) при scale 1.0/1.25/1.5/2.0: у всех `Rect` поля `fract() == 0.0`, попарных пересечений нет, сумма площадей плюс площадь зазоров равна площади `body`.
  - `layout_restores_ratios_after_shrink`: `body` 1200×800 → первая панель шире второй; `body` 200×120 → кламп по минимумам, обе стороны неотрицательны; возврат к 1200×800 даёт те же `Rect`, что до сжатия.
  - `layout_tiny_body_is_nonnegative`: `body` меньше суммы минимумов — оба `Rect` неотрицательны, покрытие сохраняется.
  - `pane_min_matches_terminal_grid_size`: при scale 1.0/1.333/2.0 и типичных `char_w`/`char_h`: `terminal_grid_size(Rect { x: 0.0, y: 0.0, ..pane_min })` даёт `raw_cols >= 10.0 && raw_rows >= 2.0`, а каждый размер минус 1 px — `raw_cols < 10.0 || raw_rows < 2.0`.
- [ ] **Step 2:** `make test-one TEST='split::tests::layout_covers_body_without_overlap'` — FAIL (модуля и функций нет).
- [ ] **Step 3:** реализация по решениям выше; `mod split;` в `src/main.rs`.
- [ ] **Step 4:** прогон каждого теста задачи через `make test-one TEST='split::tests::<name>'` — PASS; `make api-map`.
- [ ] **Step 5: коммит** `git add src/split.rs src/main.rs PROJECT_AI_MAP.txt && git commit -m "split: pure pane layout geometry"`.

### Task 2: Дерево раскладки: адресация, сплит, удаление, навигация

**Files:**
- Modify: `src/split.rs`
- Test: `#[cfg(test)] mod tests` в `src/split.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review правило преемника и выбор панели по геометрии: ничьи, вложенные узлы, панели без перекрытия

**Interfaces:**
- Produces:
  - `pub(crate) enum PaneDirection { Left, Right, Up, Down }`
  - `pub(crate) fn leaf_count(node: &SplitNode) -> usize`
  - `pub(crate) fn find_path(root: &SplitNode, pane: PaneId) -> Option<Vec<u8>>` — `0` = first, `1` = second; аллокация допустима (не hot path).
  - `pub(crate) fn node_at_path<'a>(root: &'a SplitNode, path: &[u8]) -> Option<&'a SplitNode>` и `node_at_path_mut`.
  - `pub(crate) fn split_leaf_at(root: &mut SplitNode, pane: PaneId, axis: SplitAxis, new: PaneId) -> bool` — `Leaf(pane)` заменяется на `Split { axis, ratio: 0.5, first: Leaf(pane), second: Leaf(new) }`.
  - `pub(crate) fn remove_leaf(root: &mut SplitNode, pane: PaneId) -> bool` — `false` для корня-листа и ненайденной панели; родительский `Split` заменяется оставшейся ветвью переносом `Box`, не копией.
  - `pub(crate) fn successor(root: &SplitNode, pane: PaneId) -> Option<PaneId>` — ближайший предшествующий лист в порядке обхода first→second, иначе следующий; сама `pane` не возвращается.
  - `pub(crate) fn pane_in_direction(rects: &[(PaneId, Rect)], from: PaneId, dir: PaneDirection) -> Option<PaneId>`
  - `pub(crate) struct DividerHit { pub path: Vec<u8>, pub axis: SplitAxis }`
  - `pub(crate) fn divider_at(root: &SplitNode, body: Rect, scale: f32, char_w: f32, char_h: f32, x: f32, y: f32) -> Option<DividerHit>`
- Consumes: Task 1 (`split_child_rects`, `min_extent`, `pane_min`).

Решения (§4, §6.2, §6.3):
- Порядок обхода дерева — pre-order, first затем second; по нему считаются преемник и «направление». Корень-`Split` имеет путь `[]`, его ветви — `[0]` и `[1]`.
- `pane_in_direction`: кандидат «в направлении» — `rect.x + rect.w <= active.x` (Left), `rect.x >= active.x + active.w` (Right), `rect.y + rect.h <= active.y` (Up), `rect.y >= active.y + active.h` (Down). Расстояние — вдоль оси; пересечение — длина общей части перпендикулярного интервала. Сначала кандидаты с пересечением > 0: минимальное расстояние, ничья — большее пересечение, ничья — меньший `PaneId`. Пересечения нет ни у кого — минимальное расстояние между центрами, ничья — меньший `PaneId`. Кандидатов нет — `None`.
- `divider_at`: прямоугольник каждого узла получается тем же рекурсивным проходом, что в `layout_panes` (формулы не дублировать; общий внутренний обход на `split_child_rects`). Узел подходит, если точка внутри его `Rect` и по оси попадает в зазор между ветвями: `coord` в `[first.right, second.start]` (для `Stacked` — по y); если зазор узла равен `0`, используется полоса `|coord - граница| <= (3.0 * scale).round().max(1.0)`. Из подходящих — минимальное расстояние до границы, ничья — более глубокий (`path.len()`).
- `remove_leaf` и `successor` не знают про `Tab` и фокус — чистые операции над деревом.

- [ ] **Step 1: тесты (красные)**:
  - `successor_prefers_previous_leaf_in_tree_order`: 3 листа в линии — удаление среднего даёт первый, удаление первого даёт второй, удаление последнего даёт предыдущий.
  - `split_and_remove_keep_tree_shape`: вложенное дерево; `split_leaf_at` в лист второй ветви, затем `remove_leaf` возвращает исходную форму (сравнение по `layout_panes` и `leaf_count`).
  - `remove_leaf_never_removes_root_leaf` и `split_leaf_at_reports_missing_pane`.
  - `pane_in_direction_prefers_overlap_then_distance`: «слева одна, справа две» — Left из правой верхней даёт левую; Down из правой верхней даёт правую нижнюю; Right из левой даёт верхнюю правую (большее пересечение).
  - `pane_in_direction_returns_none_at_edge_and_uses_center_distance_without_overlap`.
  - `divider_at_hits_gap_band_and_deepest_node`: две панели — точка в зазоре даёт `path.is_empty()` (корень — `[]`); точка на 10 px от зазора — `None`; вложенный узел — на его границе путь длиннее.
- [ ] **Step 2:** `make test-one TEST='split::tests::divider_at_hits_gap_band_and_deepest_node'` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** прогон каждого теста задачи через `make test-one TEST='split::tests::<name>'` — PASS; `make api-map`.
- [ ] **Step 5: коммит** `git add src/split.rs PROJECT_AI_MAP.txt && git commit -m "split: tree ops, successor and directional focus"`.

### Task 3: Модель вкладки и единая активация панели

**Files:**
- Modify: `src/app.rs` (`struct App` :516, поля :532-592; `activate_ready_terminal` :775-800 и её вызовы :817, :1178, :1180, :2279, :2297, :2340, :2775; `spawn_terminal_tab` :877; фикстуры `grid_mode_app` :4775 и другие конструкторы `Terminal::new_for_test`)
- Modify: `src/terminal.rs` (`Terminal::spawn_sized` :2196, `new_for_test` :2129, поля `Terminal`)
- Modify: `src/app/headless_run.rs` (`test_support` :160, `started_headless_with_test_terminals` :195)
- Modify: `src/app/automation.rs` (:1708), `src/app/tab_delivery.rs` (fixture :613, :1157; прямые `terminals.push` в тестах :1099, :1242) — все вызовы `Terminal::new_for_test` и все места, где тест добавляет терминал напрямую
- Test: `#[cfg(test)] mod tests` в `src/app.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review инвариант «дерево ↔ terminals», единственная точка смены фокуса и отложенная активация

**Interfaces:**
- Produces:
  - `pub(crate) struct TabId(u64)` — `Copy, Clone, PartialEq, Eq, Debug`.
  - `struct Tab { id: TabId, layout: SplitNode, active_pane: PaneId, title_number: usize }` в `src/app.rs`.
  - Поля `App`: `tabs: Vec<Tab>`, `active_tab: usize`, `next_tab_id: u64`, `next_pane_id: u64`, `structure_generation: u64`, `layout_generation: u64`.
  - `fn activate_pane(&mut self, pane: PaneId, reveal_tail: bool)`; `fn select_tab_pane_in_background(&mut self, tab: TabId, pane: PaneId)`.
  - `fn bump_structure_generation(&mut self)`, `fn bump_layout_generation(&mut self)` — первый инкрементирует оба.
  - `Terminal::pane_id(&self) -> PaneId`; `Terminal::spawn_sized(..., pane_id: PaneId)`; `Terminal::new_for_test(cols, rows, display_number, pane_id: PaneId)`.
  - `#[cfg(any(debug_assertions, test))] fn debug_assert_invariants(&self)` — доступен тестам и в release-прогоне; производственные вызовы — только под `debug_assertions`; тело проверяет инвариант через `assert!`, а не `debug_assert!` (тесты идут release-сборкой, где `debug_assert!` выключен; вне `cfg(test)` функция не компилируется, цена нулевая).
- Consumes: Task 1 (`SplitNode`, `PaneId`, `PaneId::new`).

Решения (§4):
- `pane_id` задаётся конструктором, `App::next_pane_id` выдаёт `PaneId::new(n)` и инкрементирует; переиспользование после закрытия запрещено. `TabId` выдаётся так же из `next_tab_id`.
- `Tab.layout` всегда содержит `active_pane`; `Tab.title_number` — `display_number` терминала-основателя (тот же `take_terminal_creation_number`).
- `activate_pane` находит вкладку по листу, выставляет `active_tab`, `Tab.active_pane`, `active_terminal`, при смене терминала сбрасывает `interaction`, затем исполняет тело `activate_ready_terminal`: `clear_bell_pending`, `active_terminal_presented`, `pending_tab_reveal = Some(reveal_tail)`, `send_terminal_focus_transition`.
- `activate_ready_terminal(index, reveal_tail)` остаётся тонким адаптером над `terminals[index].pane_id` — все существующие вызовы не меняются. Прямых присваиваний `active_terminal` вне `activate_pane` и индексной поправки Task 4 не остаётся (`rg -n 'active_terminal\s*='`).
- `select_tab_pane_in_background` меняет только `Tab.active_pane` — без `active_tab`, `active_terminal`, `interaction` и focus reporting.
- `spawn_terminal_tab` создаёт `Tab` с `Leaf(pane)` и делает её активной; первая вкладка — тем же путём (`spawn_initial_terminal`).
- Поколения: `structure_generation` растёт при сплите, закрытии панели, изменении состава/порядка вкладок, активной вкладке, входе/выходе сетки, изменении `grid.members`, смене метрик; `layout_generation` — на тех же событиях плюс изменение любого `ratio`. До Task 5 поля существуют, но ещё никем не читаются.
- Миграция фикстур в той же задаче: каждый `Terminal::new_for_test` получает `PaneId`; в `test_support` появляется `pub(crate) fn attach_test_terminal(app: &mut App, index: usize, terminal: Terminal) -> PaneId` — единственная операция добавления: вставляет готовый `Terminal` и создаёт/вставляет его `Tab` одним шагом на тот же индекс; `push_test_terminal(app, terminal)` — её частный случай с `index == terminals.len()`. Все фикстуры и все тесты, добавляющие терминал напрямую (`started_headless_with_test_terminals` :195, `grid_mode_app` :4775, `Fixture::new` :613, `terminals.push` и `insert` :1099, :1242 и др.), переходят на неё. Поведенческие ожидания существующих тестов не меняются.
- `debug_assert_invariants`: у каждого `Terminal` ровно один лист, каждый лист ссылается на существующий терминал, `tabs[active_tab].active_pane == terminals[active_terminal].pane_id`. Вызывается в конце завершённых операций (`activate_pane`, закрытие в Task 4), не в промежуточных шагах; объявлен под `cfg(any(debug_assertions, test))`, производственные вызовы — под `debug_assertions` (тесты идут release-прогоном `make test`, `Makefile:13`).

- [ ] **Step 1: тесты (красные)**:
  - `spawn_creates_one_tab_per_terminal_with_unique_pane_ids` (headless, 3 терминала: `tabs.len() == 3`, у каждой ровно один лист, `pane_id` уникальны, `debug_assert_invariants` не паникует).
  - `activate_pane_sets_focus_and_reports_1004`: после `activate_pane(pane_1, true)` — `active_tab == 1`, `active_terminal == 1`, `active_terminal_presented`, `pending_tab_reveal == Some(true)`, transition pair в журнале фокуса (образец существующего теста на `activate_ready_terminal`).
  - `select_tab_pane_in_background_changes_only_tab_pane`.
- [ ] **Step 2:** `make test-one TEST='app::tests::activate_pane_sets_focus_and_reports_1004'` — FAIL.
- [ ] **Step 3:** реализация; миграция всех вызовов конструктора и фикстур из Files.
- [ ] **Step 4:** тесты задачи и `make test TEST_FILTER=tab_` — PASS; существующие тесты не правятся по ожиданиям, только фикстуры.
- [ ] **Step 5: коммит** `git add src/app.rs src/terminal.rs src/app/headless_run.rs src/app/automation.rs src/app/tab_delivery.rs PROJECT_AI_MAP.txt && git commit -m "app: tab model and single pane activation path"`.

### Task 4: Закрытие панели и вкладки

**Files:**
- Modify: `src/app.rs` (`close_terminal_tab_at` :1116-1190, `try_schedule_terminal_cleanup` :1193-1212, `request_terminal_close` :1243, `busy_close_targets` :1283-1300, `cancel_close_dialog` :1312, `remove_closed_terminals` :2050-2064)
- Modify: `src/claude_close.rs` — если диалог перечисляет вкладки
- Test: `#[cfg(test)] mod tests` в `src/app.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review порядок «очистка процесса → мутация», преемник фокуса по §6.4 и состав сетки при закрытии

**Interfaces:**
- Produces:
  - `fn close_pane(&mut self, pane: PaneId) -> bool`
  - `fn close_tab(&mut self, tab: TabId) -> bool`
  - `fn remove_last_pane_of_tab(&mut self, tab: TabId)`
  - `fn remove_terminal_at(&mut self, index: usize) -> Option<PaneId>` — низкоуровневый шаг: снимает `Terminal` из `terminals`, корректирует `active_terminal`, если удалён индекс левее активного (логический фокус не меняется, 1004 не шлётся), вызывает `remove_leaf`, растит поколения Task 3; `Tab.active_pane` не трогает; возвращает преемника, если удалена сама `Tab.active_pane`, иначе `None`; `debug_assert_invariants` не вызывает. Фокусом владеют вызывающие: `close_pane` — одиночное закрытие через `activate_pane`/`select_tab_pane_in_background`; `close_tab` — пакетно ставит `Tab.active_pane` в преемника без focus reporting (§4) и один раз вызывает `activate_pane` в конце. Промежуточное состояние (`Tab.active_pane` смотрит на удалённую панель) существует только внутри одного вызова закрытия: преемник ставится в той же функции сразу после `remove_terminal_at`, между шагами не выполняется ни кадр, ни событие; `debug_assert_invariants` — только в конце составной операции.
  - `fn focus_after_removal(&mut self, removed: TabId, was_active_tab: bool, removed_was_active_pane: bool)` — процедура §6.4 п. 1–2; в конце `debug_assert_invariants` под `#[cfg(debug_assertions)]`.
- Consumes: Tasks 2 (`remove_leaf`, `successor`), 3 (`activate_pane`, `select_tab_pane_in_background`, `Tab`, поколения).

Решения (§6.4):
- `close_pane`: сначала `take_process_for_cleanup` → `try_schedule_terminal_cleanup`; `Err` (backpressure) возвращает процесс в `Terminal` и оставляет дерево, хранилище и фокус нетронутыми, возвращает `false`. Только после принятой очистки — `remove_terminal_at` и `focus_after_removal`; собственных правил фокуса у `close_pane` нет, все ветви — §6.4 (сетка открыта, сохранение вкладки, фоновая вкладка).
- `close_tab`: подтверждение строится по всем панелям вкладки (`busy_close_targets`), затем панели удаляются внутренним путём без повторного диалога и без вызова `close_pane`; отказ очистки любой панели прекращает закрытие, уже удалённые панели остаются удалёнными (спека: «вкладка остаётся с незакрытыми панелями»), один вызов фокуса в конце. `Ctrl+4` → `close_tab(active_tab)`; `cancel_close_dialog` при отмене.
- Восстановление фокуса при отказе `close_tab` — отдельная процедура (спека §6.4): сетка открыта — фокус остаётся на живой активной плите, из какой бы вкладки она ни была; если она исчезла, преемник — следующая живая плитка в порядке сетки, иначе предыдущая; сетка закрывается, только когда живых плиток не осталось, и тогда фокус идёт на `active_pane` активной вкладки. Сетка закрыта — если закрывалась активная вкладка, фокус на её `active_pane` (её могло остаться несколько панелей), иначе не меняется.
- Ветви §6.4 реализуются без сокращений:
  1. Вкладка исчезла. a. Была активной: сетка открыта — фокус в следующую плитку в порядке сетки, исключая плитки удалённой вкладки, иначе в предыдущую; плиток не осталось — сетка закрывается, фокус в `active_pane` следующей вкладки, если её нет — предыдущей; вкладок не осталось — окно закрывается. Сетка закрыта — фокус сразу в `active_pane` соседней вкладки по тому же порядку. b. Была фоновой: глобальный фокус не меняется; её плитки уходят из состава сетки, плиток не осталось — сетка закрывается.
  2. Вкладка сохранилась. a. Сетка открыта: закрытая панель была активной плиткой — следующая плитка в порядке сетки, иначе предыдущая; плиток не осталось — сетка закрывается и действует п. 2б; панель не была плиткой или была неактивной — глобальный фокус не меняется, `Tab.active_pane` по инварианту §4. b. Сетка закрыта: удалена активная панель активной вкладки — `activate_pane` выбирает преемника по дереву; удалена неактивная — глобальный фокус не меняется; удалена активная панель фоновой вкладки — `select_tab_pane_in_background`.
- `remove_last_pane_of_tab` удаляет `Terminal` и `Tab`, окно закрывается, если вкладок не осталось (существующий путь).
- `remove_closed_terminals` (:2050) удаляет ровно свою панель по `PaneId`; последняя панель вкладки — `remove_last_pane_of_tab` без подтверждения.

- [ ] **Step 1: тесты (красные)**:
  - `close_pane_gives_place_to_neighbour_and_keeps_processes_alive`.
  - `close_pane_refusal_keeps_tree_and_process`: детерминированный отказ — отключить cleanup-worker (как это делают тесты `terminal_process`, точки отказа :301/:305), затем вызвать сам `close_pane`; проверяются дерево, `terminals`, фокус и живой процесс.
  - `close_last_pane_closes_tab_and_focuses_neighbour`.
  - `close_tab_removes_all_panes_and_focuses_next_tab`, `close_tab_refusal_stops_and_keeps_remaining_panes` (удалённая до отказа панель не возвращается, оставшиеся процессы живы).
  - `natural_exit_removes_only_its_pane`.
  - `closing_active_tile_focuses_next_tile` (сетка открыта).
- [ ] **Step 2:** `make test-one TEST='app::tests::close_pane_refusal_keeps_tree_and_process'` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** прогон каждого теста задачи — PASS; `make api-map`.
- [ ] **Step 5: коммит** `git add src/app.rs src/claude_close.rs PROJECT_AI_MAP.txt && git commit -m "app: close pane and tab with focus procedure"` (`src/claude_close.rs` — только если менялся).

### Task 5: Создание сплита и раскладка панелей в App

**Files:**
- Modify: `src/app.rs` (`enum GlobalShortcut` :76, `global_shortcut` :140, `on_key` :2416, `handle_global_key` :2005, `rebuild_panels` :2128, `set_grid_metrics` :2101, `GridHint` :500, `spawn_terminal_tab` :877, сравнения поколений :1823, :2736, :2816, :2895)
- Test: `#[cfg(test)] mod tests` в `src/app.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review мутация дерева только после успешного спавна, проверка T1 на будущих прямоугольниках, поколения раскладки

**Interfaces:**
- Produces:
  - `GlobalShortcut::SplitRight`, `GlobalShortcut::SplitDown`; `GridHint::SplitTooSmall`.
  - `fn split_active_pane(&mut self, axis: SplitAxis) -> bool`.
  - Поле `App.pane_rects: Vec<(PaneId, Rect)>`.
  - `fn spawn_terminal(&mut self, wake: WakeHandle, launch: TerminalLaunchSpec, grid: (u16, u16), pane_id: PaneId) -> usize` — общая низкоуровневая часть `spawn_terminal_tab` без создания `Tab`.
- Consumes: Tasks 1–3 (поколения и `bump_*` — Task 3).

Решения (§6.1, §8.1, §8.4):
- Предикаты перехвата: `Ctrl+Shift+D`/`Ctrl+Shift+E` — при `claude_grid.is_none()` и независимо от числа листьев; `Ctrl+Shift+W` — при `claude_grid.is_none() && leaf_count > 1` (§7). В сетке клавиши уходят в терминал активной плитки.
- `split_active_pane`:
  1. Копия дерева активной вкладки (`Clone`), `split_leaf_at` на копии с новым `PaneId`, `layout_panes` в локальный `Vec` — проверка T1 по каждому листу через `pane_min`; не прошло — `set_grid_hint(GridHint::SplitTooSmall, now)`, `false`, без спавна.
  2. Спавн: `TerminalLaunchSpec { inherited_directory: terminals[active_terminal].osc7_cwd.clone(), ..Default::default() }`, `DEFAULT_SPAWN_GRID`; `PaneId` выдан до спавна и передан в `spawn_terminal`.
  3. `split_leaf_at` на настоящем дереве, `activate_pane(new, false)`, `bump_structure_generation`.
- `rebuild_panels` вне сетки: `layout_panes(&tabs[active_tab].layout, body, metrics.scale, metrics.char_w, metrics.char_h, &mut self.pane_rects)`, затем `Panel { terminal: index по pane_id, body, header: None, content_hidden: false }`; сетка открыта — прежняя ветка `grid_tiles`.
- Поколения растит каждый путь мутации Task 3–7 по своим событиям; `panel_layouts_generation` пишется по `layout_generation`; все прежние сравнения с `grid_generation` (:1823, :2736, :2816, :2895) переходят на `layout_generation`, а `grid_generation` остаётся только для состава сетки. `set_grid_metrics` растит поколения при любой смене метрик, а не только при открытой сетке.

- [ ] **Step 1: тесты (красные)**:
  - `ctrl_shift_d_splits_active_pane_into_two`: два `Panel` в `panels`, дерево `Split { SideBySide, ratio 0.5 }`, фокус на новой панели, `terminals.len()` вырос на 1.
  - `ctrl_shift_e_splits_stacked`.
  - `split_in_narrow_pane_shows_hint_and_spawns_nothing`: окно/панель уже минимума — `terminals.len()` не изменился, `tab_strip_hint` содержит «Панель слишком мала для сплита».
  - `split_uses_osc7_directory_of_active_pane`.
  - `rebuild_panels_lays_out_all_panes_without_overlap`.
  - `layout_generation_grows_on_split_and_metrics_change`.
- [ ] **Step 2:** `make test-one TEST='app::tests::ctrl_shift_d_splits_active_pane_into_two'` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** прогон каждого теста задачи — PASS; `make api-map`.
- [ ] **Step 5: коммит** `git add src/app.rs PROJECT_AI_MAP.txt && git commit -m "app: create splits and lay out panes each frame"`.

### Task 6: Строка вкладок и рамка активной панели

**Files:**
- Modify: `src/runtime.rs` (`TerminalRenderParams` :2726-2751, конструктор :3088-3112)
- Modify: `src/renderer/terminal_ui.rs` (`draw_terminal_tabs` :2504, `terminal_tab_hit_test` :3513, reveal :3549, `render_terminal_app` :3578, `render_terminal_body` :3737, `terminal_focus_separator_rect` :242)
- Modify: `src/app.rs` (конструирование параметров :2933, блок прокрутки :2885-2927)
- Modify: `src/tabs.rs` (индексы вкладок)
- Test: `#[cfg(test)] mod tests` в `src/renderer/terminal_ui.rs` и `src/tabs.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review видимая геометрия полосы вкладок и рамки на дробном scale, hit-test и drag вкладок по новым индексам, аллокации в кадре

**Interfaces:**
- Produces:
  - `pub(crate) struct TabLabelIndex { pub id: TabId, pub title_terminal: usize, pub number: usize }` в `src/runtime.rs` — без заимствованных строк: заголовок рендер берёт существующим аксессором `Terminal` по `title_terminal`, как `draw_terminal_tabs` делает сейчас.
  - `TerminalRenderParams` получает `tabs: &'a [TabLabelIndex]` и `active_tab: usize`; `terminals`/`active_terminal` остаются для тел.
  - Поле `App.tab_labels: Vec<TabLabelIndex>` — переиспользуемый буфер (`clear()` перед сборкой).
  - `pub(crate) fn pane_focus_border(body: Rect, scale: f32) -> Option<Rect>` в `src/renderer/terminal_ui.rs`.
  - `pub(crate) enum BodyChrome { Tab, GridTile, SplitPane }` — последний параметр `render_terminal_body`.
- Consumes: Task 3 (`Tab`, `TabId`), Task 5 (`panels` в сплитах).

Решения (§8.2, §8.3):
- Одна запись полосы на вкладку: `title_terminal` — индекс в `terminals` активной панели вкладки, `number` — `tab.title_number`. `draw_terminal_tabs` обходит `tabs`, заголовок читает из `terminals[title_terminal]`; `terminal_tab_hit_test` возвращает индекс вкладки.
- `src/tabs.rs` работает с индексами `App.tabs`; `active_index_after_move/remove`, drag-раскладка и reveal не меняют формул — перенос вкладки переносит все её панели.
- Рамка: `pane_focus_border` — внутренний прямоугольник с отступом `(2.0 * scale).round()`, рисуется четырьмя полосами в `render_terminal_app` вокруг тела активной панели; цвет — константа акцента, которой рисуется `terminal_focus_separator_rect`. Только при `leaf_count(tabs[active_tab].layout) > 1`.
- `render_terminal_body`: верхний сепаратор рисуется только для `BodyChrome::Tab` (:3783); в `SplitPane` его нет, границей служит рамка.
- `content_hidden`: `observe_grid_dev_warnings` (:977) вызывается и в сплитах — каждая панель получает свой флаг; `withhold_active_frame` (:955) — только одиночная вкладка.
- Прокрутка неактивных панелей: с блока :2885-2927 снимается только ограничение режима сетки (`!in_grid`); пропуск активного терминала (`panel.terminal == active`) сохраняется — у активной панели уже есть отдельное обновление, второго `scroll_y.update` за кадр быть не должно.
- Подсказка сплита рисуется тем же `grid_hint` в полосе вкладок (:3696-3709).

- [ ] **Step 1: тесты (красные)**:
  - `tab_strip_shows_one_label_per_tab_with_active_pane_title`.
  - `tab_hit_test_and_drag_use_tab_indices`: drag второй вкладки с двумя панелями переносит обе, `active_tab` следует за вкладкой.
  - `focus_border_is_inside_body_with_two_pixel_inset` (unit на `pane_focus_border` при scale 1.333/2.0 — целые пиксели).
  - `split_pane_body_has_no_top_separator` (unit на условиях `BodyChrome`).
- [ ] **Step 2:** `make test-one TEST='renderer::terminal_ui::tests::focus_border_is_inside_body_with_two_pixel_inset'` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** прогон каждого теста задачи — PASS; `make api-map`.
- [ ] **Step 5: коммит** `git add src/runtime.rs src/renderer/terminal_ui.rs src/app.rs src/tabs.rs PROJECT_AI_MAP.txt && git commit -m "render: one tab label per tab and pane focus border"`.

### Task 7: Мышь: фокус по клику и перетаскивание границы

**Files:**
- Modify: `src/app.rs` (`route_grid_pointer_button` :2718, `on_pointer_button` :2596, колесо/движение :2814-2900)
- Modify: `src/input.rs` — только если маршруту нужен `panel_layouts` панели (проверить); формулы координат не меняются
- Test: `#[cfg(test)] mod tests` в `src/app.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review глобальные координаты при hit-test, непарные release и отмена drag без отката удалённого узла

**Interfaces:**
- Produces:
  - `fn route_pointer_button(&mut self, state, button) -> bool` — расширение `route_grid_pointer_button`; активен при `grid_open() || leaf_count(active tab) > 1`.
  - `struct DividerDrag { tab: TabId, path: Vec<u8>, axis: SplitAxis, button: PointerButton, structure_generation: u64, ratio_at_start: f32 }` и поле `App.divider_drag: Option<DividerDrag>`.
  - `fn update_divider_drag(&mut self, x: f32, y: f32)`, `fn cancel_divider_drag(&mut self)`.
- Consumes: Tasks 2 (`divider_at`, `node_at_path_mut`), 3 (`activate_pane`, поколения), 5 (`pane_rects`).

Решения (§6.3, §7):
- Порядок в `route_pointer_button`: `rebuild_panels`; `Released` → `true` (завершение drag живёт в `on_pointer_button`, не здесь); `pointer_capture != None` → `true`; устаревшее поколение (`panel_layouts_generation != layout_generation`) → выбрать панель под точкой и подавить release, `false`; **`divider_at` вне сетки** → начать drag, подавить release, `false`; `panel_hit` Body → `activate_pane(pane, false)`, событие в панель, `true`; Header — как в сетке; `None` — поглотить нажатие, `false`. Проверка границы идёт раньше тела панели: при нулевом зазоре её полоса `3*scale` лежит поверх тел, и иначе drag не начался бы (§6.3).
- Поглощение release уже есть в `on_pointer_button` (:2593-2604) до маршрутизации; завершение drag вставляется туда же и только для парной кнопки (`DividerDrag.button`), иначе отпускание другой кнопки преждевременно завершило бы drag, а без завершения `divider_drag` остался бы активным и следующее движение продолжило бы тянуть границу.
- Потерянный release и вторая кнопка: нажатие той же кнопки, что в `DividerDrag`, при активном drag сначала завершает старый drag (записанное значение остаётся), затем обрабатывается как обычное новое нажатие; нажатие другой кнопки при активном drag новый drag не начинает, поглощается, а её release подавляется. Владелец drag не заменяется никогда.
- Подавление release независимо для каждой кнопки: существующее `Option<button>` (`src/app.rs:594`) заменяется на `App.suppressed_release: Vec<PointerButton>` с `fn suppress_release(&mut self, button)` и `fn take_suppressed_release(&mut self, button) -> bool`; маска `u8` не годится — `PointerButton::Other(u16)` (`src/wayland_input.rs:152`) не отображается в биты без потери идентичности, а `Vec` здесь растёт только на редких нажатиях, не на кадре. `suppress_release` идемпотентен — добавляет кнопку, только если её там нет; `take_suppressed_release` удаляет ровно одну отметку и возвращает `true`, если она была. Оба порядка отпускания (владелец первым, вторая кнопка первой) проверяются тестом, в том числе для нескольких `Other`; отдельный тест — отмена drag с последующим обычным кликом той же кнопкой доходит до терминала.
- Координаты глобальные: панель идентифицируется индексом панели в `panels`; событие получает `panel_layouts[panel_index]`, а терминал — `panels[panel_index].terminal` (массив раскладок заполняется в порядке панелей, не терминалов: `src/renderer/terminal_ui.rs:3681`); ручной пересчёт координат запрещён (§7).
- `update_divider_drag`: координата вдоль оси узла → `ratio = clamp((coord - node_start) / available)` с диапазоном минимумов (Task 1, `split_child_rects`); запись через `node_at_path_mut`; `bump_layout_generation` (не structure).
- Отмена: смена вкладки, вход/выход сетки, создание/закрытие панели, закрытие вкладки, смена метрик — восстановление `ratio_at_start` до структурной мутации, если узел пути ещё существует; сброс состояния и подавление парного release.
- Колесо и движение — панели под указателем (при захвате — захватившей); фокус от них не меняется.
- Завершение отпусканием кнопки оставляет записанное значение; курсор и подсветка границы не меняются (§6.3).

- [ ] **Step 1: тесты (красные)**:
  - `click_on_second_pane_focuses_it_and_delivers_press`.
  - `click_in_gap_starts_divider_drag_and_changes_no_focus`.
  - `click_near_border_starts_drag_when_node_narrower_than_gap`: узел уже зазора (полоса `3*scale` лежит поверх тел) — нажатие в полосе начинает drag, а не уходит в панель.
  - `divider_drag_finishes_on_release_and_motion_does_not_move_it`: после отпускания движение указателя `ratio` не меняет.
  - `lost_release_does_not_leave_drag_active`: Press той же кнопки при живом drag завершает старый и начинает новый по правилам маршрута.
  - `second_button_press_does_not_replace_drag_owner`: другая кнопка при активном drag его не заменяет, поглощается, её release подавляется; оба порядка отпускания (владелец первым и вторая кнопка первой) не дают ни одного release попасть в терминал, в том числе для двух разных `PointerButton::Other`.
  - `cancelled_drag_then_normal_click_reaches_terminal`: после отмены drag (смена вкладки) обычный клик той же кнопкой доходит до терминала — отметка подавления не осталась.
  - `divider_drag_resizes_ratio_and_both_ptys` (по `panel_layouts` и размеру grid обеих панелей).
  - `stale_layout_generation_only_selects_pane_and_suppresses_release`.
  - `drag_cancelled_by_tab_switch_restores_ratio`.
  - `wheel_and_motion_follow_panel_under_pointer`.
- [ ] **Step 2:** `make test-one TEST='app::tests::divider_drag_resizes_ratio_and_both_ptys'` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** прогон каждого теста задачи — PASS; `make api-map`.
- [ ] **Step 5: коммит** `git add src/app.rs src/input.rs PROJECT_AI_MAP.txt && git commit -m "input: pane focus by click and divider dragging"` (`src/input.rs` — только если менялся).

### Task 8: Фокус Alt+стрелками и закрытие панели с клавиатуры

**Files:**
- Modify: `src/app.rs` (`enum GlobalShortcut` :76, `global_shortcut` :140, `exact_global_modifiers` :133, `on_key` :2416, `handle_global_key` :2005)
- Test: `#[cfg(test)] mod tests` в `src/app.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review перехват Alt+стрелок: одиночная вкладка и сетка не меняются, точная проверка модификаторов

**Interfaces:**
- Produces: `GlobalShortcut::FocusPane(PaneDirection)`; `GlobalShortcut::ClosePane`; `fn exact_alt_modifiers(modifiers) -> bool` — Alt без Ctrl, Shift и Super.
- Consumes: Tasks 2 (`pane_in_direction`, `leaf_count`), 3 (`activate_pane`), 4 (`close_pane`), 5 (предикаты перехвата).

Решения (§6.2, §7):
- Распознавание: `Alt+←/→/↑/↓` — Alt без Ctrl и Shift (`exact_alt_modifiers`; существующий `exact_global_modifiers` требует Ctrl и для стрелок не годится — не менять его); перехват в `on_key` только при `claude_grid.is_none() && leaf_count(active tab) > 1`, иначе событие идёт обычным маршрутом в терминал.
- Действие фокуса: `rebuild_panels` (если кадр ещё не отрисовал), `pane_in_direction(&self.pane_rects, active_pane, dir)`, найденная панель — `activate_pane(pane, false)`; не найдена — фокус не меняется.
- `Ctrl+Shift+W` — `GlobalShortcut::ClosePane`, перехват по тому же предикату, действие — `close_pane(active_pane)`.
- В сетке все эти клавиши не перехватываются и уходят в терминал активной плитки.

- [ ] **Step 1: тесты (красные)**:
  - `alt_left_moves_focus_to_left_pane`, `alt_down_moves_focus_to_lower_pane`.
  - `alt_arrows_go_to_terminal_when_single_pane` (байты доходят до shell).
  - `alt_arrows_not_intercepted_in_grid`.
  - `ctrl_shift_w_closes_active_pane_only_with_two_panes`.
- [ ] **Step 2:** `make test-one TEST='app::tests::alt_left_moves_focus_to_left_pane'` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** прогон каждого теста задачи — PASS; `make api-map`.
- [ ] **Step 5: коммит** `git add src/app.rs PROJECT_AI_MAP.txt && git commit -m "keys: Alt arrows focus panes, Ctrl+Shift+W closes pane"`.

### Task 9: Claude-сетка и tab-status на панелях

**Files:**
- Modify: `src/app.rs` (`tab_visible` :2221, `collect_claude_members` :2197, `select_grid_tile`, вход/выход сетки)
- Modify: `src/app/tab_delivery.rs` (`handle_tab_status` :115, использование `tab_visible` в `handle_tab_wait`)
- Test: `#[cfg(test)] mod tests` в `src/app/tab_delivery.rs`

**Агенты:** implementer ds-low-agentic · reviewer ds-high — high: task review семантика «панель на экране» для фоновых вкладок и состав сетки из панелей

**Interfaces:**
- Produces: `fn tab_visible(&mut self, index: usize) -> bool` с новой семантикой (сигнатура существующая).
- Consumes: Tasks 3–7.

Решения (§9, §10):
- `tab_visible(index)`: сетка открыта — панель состоит в `claude_grid.members`; иначе — вкладка панели активна и `active_terminal_presented`.
- Плитка — панель: `collect_claude_members` по-прежнему возвращает индексы `terminals` с `claude_foreground`; смешанная вкладка (Claude + shell) даёт одну плитку.
- `select_grid_tile` → `activate_pane(pane, reveal)`.
- `handle_tab_status`: `focused` — фокус окна, `active` — `tab_visible`; `TAB_INPUT` и `find_tab_by_token` не меняются — панель уже отдельная цель по токену, `/clear` уходит в панель своей сессии.
- `tab-wait` использует ту же `tab_visible`; протокол не меняется.
- Выход из сетки возвращает активную вкладку с её деревом; состав и раскладка сетки не меняются от сплитов.
- Внешний запуск (single instance) по-прежнему открывает новую вкладку, не панель.

- [ ] **Step 1: тесты (красные)**:
  - `two_panes_same_tab_both_report_active_when_tab_presented`.
  - `tab_input_goes_to_its_own_pane_by_token` (разные маркеры в выводе).
  - `clear_input_reaches_pane_of_its_own_session` (`/clear` по токену панели при двух панелях).
  - `token_addressing_survives_tab_drag_and_other_tab_close` (адрес по токену после drag вкладки и закрытия другой вкладки).
  - `tab_wait_resolves_when_pane_visible_and_reports_gone_after_close`.
  - `mixed_tab_gives_one_tile_and_exit_restores_split`.
- [ ] **Step 2:** `make test-one TEST='app::tab_delivery::tests::two_panes_same_tab_both_report_active_when_tab_presented'` — FAIL.
- [ ] **Step 3:** реализация.
- [ ] **Step 4:** прогон каждого теста задачи — PASS; `make api-map`.
- [ ] **Step 5: коммит** `git add src/app.rs src/app/tab_delivery.rs PROJECT_AI_MAP.txt && git commit -m "harness: pane visibility for tab-status and tab-wait"`.

### Task 10: Приёмка: dump панелей и вкладок, headless-сценарии, снимки, скорость

**Files:**
- Modify: `src/headless/dump.rs` (:141, :151, :155-157, :225-231)
- Create: `src/headless/tests_split.rs`; объявление в `src/headless/mod.rs` (:303-316)
- Modify: `docs/headless.md` (формат dump)
- Артефакты: снимки и лог бенча в `.superpowers/sdd/2026-11-08-ronsole-split-panes/shots/`

**Агенты:** implementer ds-low-agentic · reviewer нет (лист без потребителей; ловит финальное ревью ветки)

**Interfaces:**
- Produces: в `dump` активного терминала — `active_pane` (`PaneId::raw`) и `panels: [{ pane_id, x, y, w, h, content_hidden }]`; элемент `tabs[]` — по одному на `App.tabs` (его hitbox, `title_number`), а не на `Terminal`; имена и типы существующих полей не меняются.
- Consumes: Tasks 1–9.

Решения (§12):
- `tests_split.rs` — сценарии через `session_with_shell` и `run_script` (`src/headless/tests.rs:24`, :63): `key ctrl+shift+d` → dump даёт 2 панели без пересечений и с зазором ровно `gap`; `key ctrl+shift+e` → 3 панели; `key ctrl+shift+w` → 2; `key alt+left` меняет `active_pane`; `screenshot <path>` пишется в `shots/`.
- Снимки для глаз: один скрипт через `python3 scripts/ronsole_headless.py run -` (строки протокола: `key ctrl+shift+d`, `key ctrl+shift+e`, `screenshot <путь>`) на `--size 2560x1440 --scale 1.333` — раскладки 2 и 3 панелей (вертикально, горизонтально, вложенно), пустой и заполненный терминал, наведение и нажатие на границу. Точный синтаксис CLI сверить `python3 scripts/ronsole_headless.py --help` до запуска. PNG открываются как изображение (правило `~/projects/ronsole/CLAUDE.md`, «Приёмка UI»): зазор и рамка ровные, панели не перекрыты, ничего не съехало.
- Скорость: замеры — протокольными командами внутри той же сессии `run -`, где скриптом созданы сплиты: `bench <N> key alt+left`, `bench <N> type <text>` и `bench <N> wheel 0 -3` при указателе над телом панели (подкоманда `bench` скрипта поднимает отдельную сессию без раскладки и для этого не годится; у `wheel` обязательны `dx dy`); точный синтаксис сверить `python3 scripts/ronsole_headless.py --help` и `docs/headless.md`. p99 `total_ms` ≤ 4.17 мс; частое действие ≤ 70 мс от ввода до кадра. Перед бенчем проверить, что на GPU нет игры (`nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader`, `ps -eo comm=`).
- Финальный гейт: `make codex_test`, `make api-map`, `git diff --check`; снимки и числа — последней строкой отчёта.

- [ ] **Step 1:** тест `headless_split_panes_screenshot_and_dump` — FAIL (нет полей `panels`, `tabs[]` по терминалам).
- [ ] **Step 2:** реализация dump и теста.
- [ ] **Step 3:** `make test-one TEST='headless::tests_split::tests::headless_split_panes_screenshot_and_dump'` — PASS.
- [ ] **Step 4:** `make fast`, затем снимки и `bench` в `shots/`; открыть PNG и проверить глазами.
- [ ] **Step 5:** `make codex_test`, `make api-map`, `git diff --check` — все зелёные.
- [ ] **Step 6: коммит** `git add src/headless/dump.rs src/headless/tests_split.rs src/headless/mod.rs docs/headless.md PROJECT_AI_MAP.txt && git commit -m "headless: dump panes and split-pane acceptance"`.
