# Headless-режим Ronsole — спека

Дата: 23–24.10.2026. Путь brainstorming: architectural (новая подсистема), оценка исполнения больше часа.

## 1. Цель

Ronsole запускается без окна и без Wayland: рисует тем же `Renderer` во внеэкранный буфер, принимает команды по текстовому протоколу и отдаёт состояние (`dump`) и снимки (PNG). На этом режиме строятся:

* тесты UI на каждую возможность терминального паритета L1–L17 и L20 (список — §9);
* PGO-тренировка и замер памяти — без окна на экране пользователя (сейчас `scripts/pgo_pipeline.py` и `measure-rss.sh` запускают настоящий Ronsole с `WAYLAND_DISPLAY` пользователя);
* сверка паритета Ronsole ↔ RRiter (план RRiter `/home/reyan/projects/rriter/docs/superpowers/plans/2026-10-21-terminal-parity-rriter.md`, Task 8): общее тело сценария, перед которым у каждого приложения свой пролог (открыть терминал, задать шелл).

Успех: `make test` гоняет headless-сценарии без Wayland-сессии; `pgo_pipeline.py` тренирует без `WAYLAND_DISPLAY`; общее тело сценария из `scripts/term_parity/` RRiter с прологом Ronsole исполняется драйвером Ronsole без правок тела.

Вне цели: headless на macOS/Windows (Ronsole — только Linux/Wayland); общий crate headless для двух приложений (Q10); виртуальные часы (Q14); эталонные PNG (Q13).

## 2. Принятые решения

| # | Вопрос | Решение |
|---|--------|---------|
| Q1 | Ctrl+клик по `file://` на исполняемый файл | Не к этой спеке: вариант O2, спека терминального паритета 6.3 (Ronsole PR #5). |
| Q2 | Кто ревьюит план RRiter | Не к этой спеке: `final-reviewer` + `ds-high` по списку L1–L20. |
| Q3 | Порядок работ | Headless Ronsole — до исполнения плана паритета RRiter. |
| Q4 | Покрытие | Headless-сценарий на каждую возможность L1–L20, кроме L18 (замер) и L19 (ручная проверка): настоящий шелл и `printf`, проверка через `dump` и пиксели PNG. |
| Q5 | PGO | Только headless, как в RRiter (`scripts/pgo_pipeline.py:752` RRiter); Wayland-тренировка удаляется. |
| Q6 | Где живёт режим | Флаг `--headless` в обычном release-бинаре, не отдельная сборка. |
| Q7 | Установка PGO-бинаря | Сейчас не ставить; ставить в `~/bin/system/ronsole` только после headless-PGO (§8). |
| Q8 | Шрифт терминала RRiter | Не к этой спеке: план RRiter, Global Constraints. |
| Q9 | Модификатор ссылок на macOS в RRiter | Не к этой спеке: план RRiter, Global Constraints. |
| Q10 | Как устроить headless | Как в RRiter, внутри Ronsole: EGL surfaceless + pbuffer, тот же `Renderer`, безоконный runtime вместо Wayland. Не headless-композитор, не общий crate. |
| Q11 | Протокол | Те же команды и ответы, что у RRiter (`ok`/`err`, stdin или `--script`); драйвер `scripts/ronsole_headless.py` по образцу `rriter_headless.py`. |
| Q12 | Состав `dump` | JSON: видимый текст, курсор, заголовок, режимы (DECCKM, bracketed paste, mouse tracking, Kitty, 1004, 2026), выделение, наведённая ссылка, вкладки; атрибуты ячеек — по запросу строки. |
| Q13 | Проверка PNG | Цвет пикселя в точках клеток; эталонных PNG нет. |
| Q14 | Время | Реальное, ожидание по условию (`wait_until`), как в RRiter. |
| Q15 | Форма курсора в `dump` (принято при записи спеки) | Ключа формы нет: Ronsole не хранит форму курсора (DECSCUSR не поддержан, курсор — фиксированный прямоугольник, `src/renderer/terminal_ui.rs:3003-3010`). Ключ `shape` появится вместе с DECSCUSR. |
| Q16 | Удержание модификаторов для Ctrl-наведения и Ctrl+клика (принято при записи спеки) | Новая команда `mods` (§4.2) в Ronsole и в RRiter (план RRiter, Task 8): в протоколе RRiter модификатор есть только у `click … alt`, а L9 и L15 требуют Ctrl без движения мыши и Ctrl+клик. |
| Q17 | Single-instance в headless (принято при записи спеки) | Headless не подключается к сокету single-instance и не создаёт его: иначе тестовый запуск передал бы ввод в Ronsole пользователя. |
| Q18 | Проверка пробуждений цикла (принято при правке по ревью) | Строгая модель RRiter: команды `wake <ms>` и `idle <ms>` делают шаг, только когда Wayland-цикл проснулся бы (eventfd, срок, кадр). Потерянное пробуждение видно как `cause=timeout`. `settle`/`wait` шагают по расписанию драйвера и пробуждений не проверяют. |
| Q19 | Нелатинская раскладка в `key` (принято при правке по ревью) | Опция `key --char C`: символ раскладки на физической клавише (`unshifted` и текст), keycode — по имени клавиши. Без `--layout`. То же в RRiter (план RRiter). |
| Q20 | Координаты мыши в общих сценариях (принято при правке по ревью) | Команда `mouse_cell <col> <row>` — указатель в центр клетки видимой области активного терминала, в Ronsole и RRiter. Общее тело сценария пиксельных `mouse_move` не содержит: размеры клеток у приложений разные. |
| Q21 | `--pgo-train` в headless (принято при правке по ревью) | Отдельный режим без протокола: stdin не читается, цикл крутится до выхода автоматизации, код выхода — по её итогу. |
| Q22 | `external_request` (принято при правке по ревью) | Контракт RRiter `{"kind","arg"}`, `dump` читает и очищает. Ronsole отдаёт только `open_url`; `arg` — строка, которая ушла бы в `xdg-open` (для O2 — `file://<каталог-родитель>`). RRiter для того же O2 отдаёт `reveal_path(<файл>)`; сверка паритета считает `reveal_path(p)` ≡ `open_url(file://<родитель p>)` (нормализация в `compare.py` плана RRiter: Ronsole канонизирует путь и кодирует URI, `platform/process.rs:375-397`, RRiter отдаёт путь как есть — сравниваются канонизированные пути без процентного кодирования). Так же для обычного файла и каталога: RRiter отдаёт `open_path(<канонический путь>)`, сверка считает `open_path(p)` ≡ `open_url(file://p)` по тем же правилам (решение владельца 25.10, финальное ревью Task 8 RRiter, D1). Остальные запросы сравниваются как есть. |
| Q23 | Владелец EGL display (принято при правке по ревью) | `OffscreenGl`, без static-счётчика RRiter (`offscreen_gl.rs:8` RRiter); `eglTerminate` не вызывается, display живёт до конца процесса, `libEGL` открывается с `RTLD_NODELETE` (его `dlclose` в `Drop` не выгружает библиотеку); сессий в процессе — сколько угодно, каждое GL-действие сессии идёт при её текущем контексте (§3.2). |
| Q24 | Система координат терминала в протоколе (принято при правке по ревью) | Видимая область — `rows` строк, начиная со строки `top`, верхней строки, которую рендер рисует с учётом прокрутки (§5.1), без overscan; при прокрутке не на целую строку нижняя строка обрезана на долю строки. Формулу `top` пользователь исправил 25.10 по финальному ревью: прежняя `bottom = total − 1 − floor(scroll_offset / char_h)` при высоте текста не на целое число строк не доходила до строки 0, которую рендер рисует. Все строки в `dump`, `dump row`, `mouse_cell` отсчитываются от её верха, колонки — от 0; концы отрезков ссылок исключающие, у выделения — включающий `last`. |
| Q25 | Ошибка `resize` в headless (принято при правке по ревью, круг 2) | Headless `runtime.resize` сам меняет pbuffer, если размер другой (так resize из автоматизации PGO доходит до поверхности). Команда `resize` протокола сначала меняет pbuffer, и только при успехе зовёт App; при ошибке ни App, ни конфиг профиля, ни цель рисования не меняются, ответ `err`. Ошибка pbuffer внутри `runtime.resize` (путь автоматизации) записывается в сессию, `--pgo-train` завершается с кодом 1. Wayland-путь без изменений. |
| Q26 | Состав общего тела сценария (принято при правке по ревью, круг 2) | Общее тело не использует пиксельные координаты и `dump.tabs[].rect`: клик по вкладке — в прологе или в тестах только Ronsole. Часть L10 про 1004 через переключение вкладок — тест Ronsole, не общее тело. |
| Q28 | Этапы (пользователь, 24.10) | Два этапа, два плана (§12): этап 1 — PGO без окна, после него сборка и установка PGO-бинаря (Q7); этап 2 — протокол, `dump`, тесты L1–L20, замер памяти, сверка с RRiter. Q3 относится к этапу 2. |
| Q27 | Внешние эффекты в `--pgo-train` (принято при правке по ревью, круг 2) | Как в протоколе: записывающие варианты (буфер `Memory`, перехват открытия ссылок) и временный профиль §3.4. Иначе тренировка без Wayland трогала бы системный буфер и запускала `xdg-open`. |
| Q29 | Колонки фаз редактора RRiter `root_*`/`chrome_*` в `bench` | Опущены: в Ronsole таких фаз нет (пользователь, 24.10). |

## 3. Архитектура

### 3.1. Шов runtime

Сейчас `WindowRuntime` (`src/runtime.rs:2653`) держит поля в порядке `renderer, config, context, surface, wayland: Box<DirectWaylandBackend>, wake, requested_plan, requested_priority, swap_interval_applied` (`runtime.rs:2654-2662`); единственный конструктор — `bootstrap_wayland` (`runtime.rs:2666-2721`: Wayland connect и configure → EGL display, config, context, window surface → `Renderer::new(create_glow_context(..))` → `WakeHandle::new` (eventfd)). `Renderer::new` (`src/renderer.rs:492`) принимает только `glow::Context`.

Решение: `WindowRuntime` делится на GL-часть и оконную часть.

* **GL-часть** — `Renderer` и цель рисования одного из двух видов: окно (glutin config, context, EGL window surface на `wl_surface`, как сейчас) или `OffscreenGl` (§3.2). Порядок уничтожения прежний: сначала `Renderer` (его GL-объекты удаляются при живом контексте), затем цель (surface, context), затем оконная часть (`wl_surface` переживает EGL window surface), последним eventfd.
* **Оконная часть** — `Wayland(DirectWaylandBackend)` или `Headless(HeadlessBackend)`.
* Конструкторы: `bootstrap_wayland` (поведение без изменений) и `bootstrap_headless(size, scale, terminal_font_size, terminal_background)`.

Каждая точка шва, которую сейчас зовут App, цикл или автоматизация:

| Точка (`runtime.rs`) | Wayland — как сейчас | Headless |
|---|---|---|
| `wayland_metrics` (2724; читатели `direct_wayland.rs:36,121`, `app/automation.rs:1038`, `physical_size` 2889 → `diagnostics_report` 2894) | метрики configure и scale | тот же тип метрик: физический размер — текущий размер pbuffer (`--size`, затем `resize` протокола или автоматизации), логический = физический / масштаб, масштаб из `--scale`/`scale` |
| `request_frame` (2732 → 2399-2402: `FrameSchedule::request`, frame callback) | как сейчас | флаг «кадр запрошен», счётчик `frame_requests` += 1 (§5.1) |
| `take_wayland_frame_ready`, `wayland_frame_ready_requested` (2740, 2744) | готовность по frame callback | кадр готов, если запрошен и с прошлого нарисованного прошло ≥ 8 мс (темп `HEADLESS_FRAME_PACE` RRiter, иначе поток PTY займёт ядро); принудительный шаг команды темп не ждёт |
| `render_terminal_and_present` (2796: рендер 2801, `swap_buffers` 2818-2820, `mark_presented` 2824) | рендер, затем present цели: `swap_buffers` + `mark_presented` | рендер тот же; present без swap — время кадра и счётчик `frames`; буфер остаётся для `screenshot` |
| `poll_wayland(timeout_ms)` (2748: `poll(2)` по fd Wayland и eventfd, 2488-2520) | как сейчас | `poll(2)` только по eventfd; таймаут — минимум переданного (`-1` — без срока, `runtime.rs:278-281`) и срока темпа `last_frame + 8 мс`, если кадр запрошен и ещё не нарисован (замена frame callback Wayland, `runtime.rs:845-880`, 2417-2427). Истечение срока темпа — кадр готов, как по frame callback; тот же `WaylandPollOutcome { woke, timed_out }` |
| `take_wayland_events` (2753) | события Wayland | пусто |
| `wayland_input_deadline`, `process_wayland_input_deadline` (2757, 2761: автоповтор) | как сейчас | `None` и ничего: повтор — только явный `key --repeat N` |
| `acknowledge_wake` (2736; зовёт только `on_frame`, `app.rs:1819`) | `wake.drain()` | то же |
| `wake_handle` (2728) | клон eventfd | то же; поток PTY будит через `notify_main_loop` (`terminal_process.rs:943-952`) без изменений |
| `set_cursor_kind(&self)` (2854; `Cell<CursorKind>`, `runtime.rs:983`) | `wp_cursor_shape` | `Cell<CursorKind>` для `dump.cursor_icon` |
| `activate_existing_window` (2765) | как сейчас | ничего (не вызывается: single-instance нет, Q17) |
| `resize` (2785: `surface.resize` + `renderer.resize`) | как сейчас | `OffscreenGl::resize`, если `size()` отличается, затем `renderer.resize`. После команды `resize` протокола pbuffer уже нужного размера (Q25, §4.2), и шаг пустой; resize из автоматизации PGO (`app/automation.rs:694`) меняет pbuffer здесь; при ошибке `renderer.resize` не зовётся, ошибка записывается в сессию (§3.3, `--pgo-train`) |
| `update_scale_factor`, `set_terminal_font_size`, `set_terminal_background`, `reset_fps_counter`, hit-test и раскладка вкладок (2769-2884) | только `Renderer` | общие, через GL-часть |
| `diagnostics_report` (2894) | как сейчас | то же плюс `headless pbuffer` |

Транзакционного resize в Ronsole нет и в headless не вводится: `resize` и `scale` применяются целиком до следующего кадра тем же путём, что Wayland `Configured` (`direct_wayland.rs:96-105`) и `on_scale_changed` (`app.rs:1511-1516`). Единственный шаг, который может отказать, — новый pbuffer; у команд протокола он идёт первым, до App (Q25): `OffscreenGl::resize` создаёт новый pbuffer и уничтожает старый только при успехе, при ошибке команда отвечает `err` и App не зовёт, так что конфиг профиля (`app.rs:1500-1504`) и размеры App остаются прежними.

Ввод в headless идёт методами App, которые зовёт Wayland-цикл (`direct_wayland.rs:107-118`): `on_key` (`app.rs:1544`), `on_ime_commit` (1528), `on_modifiers` (1522), `on_pointer_motion` (1607), `on_pointer_leave` (1678), `on_pointer_button` (1699), `on_scroll` (1793), `on_focus_changed` (949), `on_resize_with_logical_size` (1489), `on_scale_changed` (1511). `on_text` (1584) протокол не зовёт: Wayland его тоже не зовёт; автоматизация PGO зовёт его сама (`app/automation.rs:699`), как в окне. Новый путь ввода не вводится; методы становятся `pub(crate)`.

Новый код — в `src/headless/` (протокол, сессия и цикл, `dump`, профиль, bench, тестовая обвязка) и `src/platform/offscreen_gl.rs`. `runtime.rs` (3445 строк) и `app.rs` (3831) получают только шов и маршрутизацию, без логики headless.

### 3.2. Внеэкранный GL

`src/platform/offscreen_gl.rs` переносится из RRiter (259 строк; публичный API `new(width, height) -> Result<Self, String>`, `glow()`, `resize()`, `requested_context()`, `gl_strings()`, `size()`; `dlopen("libEGL.so.1")`, `EGL_MESA_platform_surfaceless`, pbuffer, OpenGL 3.3 Core). Отличия от RRiter: без `StartupTrace` (`new_traced` не переносится), без тестового блока, зависящего от типов RRiter, и без `static DISPLAY_USERS` (Q23): `Drop` уничтожает surface и context, `eglTerminate` не вызывается — display принадлежит процессу и общий для всех сессий в нём; `libEGL.so.1` открывается с `RTLD_NODELETE`, поэтому `dlclose` в `Drop` (у RRiter `offscreen_gl.rs:16-22`) не выгружает библиотеку и не портит display других сессий. Повторный `eglGetPlatformDisplay` той же платформы отдаёт тот же display, повторный `eglInitialize` — без эффекта. Каждое GL-действие сессии идёт при её текущем контексте: метод GL-части, который зовёт GL (кадр, `screenshot`, `resize`, `update_scale_factor` и прочие методы `Renderer` из строки таблицы §3.1, `Drop` `Renderer`), сначала делает текущим контекст своей сессии (`src/renderer.rs:627`, `:1253` — GL-вызовы вне кадра).

Вендор EGL и аллокатор: ветка headless сама вызывает `prefer_egl_vendor` (`src/main.rs:69-89`) до первой загрузки EGL и `tune_glibc_allocator` (`main.rs:111-120`) — в обычном пути они стоят после single-instance (`main.rs:179-180`), до которого headless не доходит. Без второго замер памяти (§7) и профиль PGO (§8) снимались бы с другой настройкой malloc.

Ошибка EGL — выход с кодом 3 и строкой `headless: EGL setup failed at <stage>: <detail>` в stderr (как в RRiter; один формат во всей спеке).

Чтение пикселей для `screenshot` — `glReadPixels` из текущего буфера, запись PNG (крейт `png` или `image`, если уже есть в зависимостях; иначе `png` — в плане).

### 3.3. Цикл

Тело итерации Wayland-цикла (`src/app/direct_wayland.rs:139-224`: события backend → `on_about_to_wait` → кадр, если готов → таймаут из `AppLoopControl` (`app.rs:1335-1362`) и срока ввода → ожидание backend → при `woke` `take_frame_wake` → `request_frame`) выносится в одну функцию шага. Её зовут Wayland-цикл и headless; копии логики из `direct_wayland.rs` нет (правило «эффект — в одной точке»). Вывод PTY App забирает в `on_about_to_wait` (`app.rs:1934-1935`) на каждом шаге.

В режиме протокола между командами ничего не выполняется (как в RRiter): команда читается, исполняется, ответ печатается.

* Команда ввода вызывает методы App (§4.2) и делает принудительный шаг: `on_about_to_wait` и кадр без ожидания и без темпа. Каждое синтезированное событие — ровно один кадр, `click` и `key` — два (нажатие, отпускание), как в RRiter (`docs/headless.md` RRiter, Rules).
* `settle [ms]` (по умолчанию 500): шаги, пока два подряд не окажутся простоями без запрошенного кадра и без срока раньше бюджета; ответ `ok frames=<n> settled=<bool>`.
* `wait <ms>` (≤ 60000): шаги по реальным часам до срока; между шагами — ожидание backend с таймаутом ≤ 2 мс; ответ `ok frames=<n>`.
* `wake <ms>` (Q18): один шаг строго как Wayland-цикл — ожидание backend по eventfd, сроку App и сроку темпа запрошенного кадра (§3.1, `poll_wayland`), без кадров драйвера; кадр рисуется, только если шаг его запросил. Причина `cause`: `redraw` (кадр запрошен и стал готов — сразу или по сроку темпа), `poll` (`AppLoopControl::Poll`), `ui_waker` (сработал eventfd), `deadline` (срок `WaitUntil` прошёл), иначе `timeout` через `ms`. Ответ, как у RRiter: `ok cause=<c> frame=<bool> flow=<wait|poll|wait_until> deadline_ms=<n|none>`.
* `idle <ms>`: `wake`, повторённый в течение `ms` без ввода; ответ `ok frames=<n> redraws=<n> polls=<n> wakes=<n> deadlines=<n> flow=<f> deadline_ms=<n|none>`.
* Курсор не мигает (мигания в Ronsole нет); простаивающее приложение кадров не рисует и кончает `idle` с `flow=wait deadline_ms=none`.

Выход App: `AppLoopControl::Exit` из `on_about_to_wait` (закрыта последняя вкладка, `app.rs:1940-1943`) или `true` из `on_key` (`CloseTab` на последней вкладке, `app.rs:1409-1411`) или из `on_pointer_button` (крестик последней вкладки, `TerminalTabHit::Close`, `app.rs:1736`; Wayland выходит по нему же, `direct_wayland.rs:126-131`) — headless вызывает `on_exiting` (`app.rs:1968-1971`), отвечает на текущую команду и завершается; следующие команды не читаются. Код выхода — как в RRiter: `0` все ответы `ok`, `1` был `err` или нечитаемый ввод, `2` неверные аргументы, `3` EGL или `Renderer`.

Режим `--pgo-train` (Q21): протокол не читается, stdin не открывается; внешние эффекты и профиль — как в протоколе (Q27, §3.4). Функция шага крутится в цикле с ожиданием backend (кадры по темпу 8 мс); автоматизация продвигается в `on_about_to_wait` (`app.rs:1952-1965`) с дедлайном `loop_control_with_automation_deadline` (`app.rs:1352-1362`), как в окне. После каждого шага цикл проверяет ошибку сессии (pbuffer в `runtime.resize`, Q25): есть — `interrupt_automation(<причина: resize pbuffer …>)` (`app/automation.rs:1573`, `:1326-1340`; отчёт `failed`, код 1); `failed_step` в отчёте — шаг автоматизации, текущий на момент проверки (после `resize-*` это `wait-resize-*`), поэтому тест проверяет причину, а не имя шага; дальше как по `Exit`. `interrupt_automation` и `take_automation_exit_result` сейчас `pub(super)` — headless зовёт их через шов в `app.rs` или они становятся `pub(crate)`. По `Exit` — `take_automation_exit_result`, `on_exiting`; код `0` при отчёте `ok`, `1` при ошибке шага, таймауте или записи отчёта (как `main.rs:193-199` сейчас), `3` при ошибке EGL — с отчётом о сбое старта (`write_automation_startup_failure`, `app/automation.rs:43-62`).

### 3.4. Изоляция

* Профиль — временный каталог `${XDG_RUNTIME_DIR:-/tmp}/ronsole-headless-<pid>[-<n>]` (0700), удаляется при выходе, кроме `--keep-profile`; `--profile DIR` — свой каталог, не удаляется. В нём конфиг (`config` вместо `$XDG_CONFIG_HOME/ronsole/config`, `src/config.rs:175-176`); запись конфига при resize (`on_resize_with_logical_size`, `app.rs:1500-1504`) уходит в профиль. Путь передаётся в `App` параметром (шов `platform::config_home_dir_with`, `src/platform.rs:70`), без изменения переменных окружения процесса.
* Single-instance (Q17): ветка headless в `main` стоит до `acquire_single_instance` (`main.rs:149`); сокет `$XDG_RUNTIME_DIR/ronsole` не открывается и не создаётся.
* Внешние эффекты — у App владельцы с двумя вариантами: системный (как сейчас) и записывающий (headless). Вариант выбирается параметром при создании App; глобального состояния нет.
  * Буфер обмена. Сейчас `TerminalInteraction.clipboard: Option<Clipboard>` (`src/input.rs:15-19`, ленивое создание `:339-344`, обёртка над `arboard` `src/platform.rs:83-98`). Он становится владельцем `System` (ленивый `arboard`, как сейчас) или `Memory { text: Option<String> }`. Через него идут все обращения: запись — копирование выделения Ctrl+C (`input.rs:538-558`), поиск (`:440`, `:447`), `set_clipboard_text` (`:351-354`) с OSC 52 (`terminal.rs:1743-1744`), настройки (`app.rs:1069-1071`); чтение — вставка Ctrl+V (`input.rs:508-523`: `get_file_list`, затем `get_text`), поиск (`input.rs:438-455`), настройки (`app.rs:1055-1058`). `Memory`: `get_text` отдаёт последнюю запись или `None`, `get_file_list` — `None`. `dump.clipboard = {"mode":"memory","text":…}`, `text` — `null` до первой записи. Системный буфер не трогается. Чтение OSC 52 Ronsole не поддерживает (`osc.rs:102-104`) — без изменений.
  * Открытие ссылок (`app.rs:1788` → `platform/process.rs:349-358`, решение в потоке): решение O2 (`local_open_decision`, `opener_argument`) исполняется как обычно; вместо запуска `xdg-open` запрос `OpenUrl(<аргумент>)` уходит в записывающий приёмник — канал, как `ExternalRequestLog` RRiter (`src/platform/desktop_request.rs:54-83` RRiter). Запрос приходит из потока, поэтому тест ждёт его `wait_until` по `dump`, а не читает сразу после `click` (§5.1, Q22).
* Шелл: по умолчанию как сейчас (`$SHELL`, затем `/bin/bash`, `/bin/sh`, `terminal_process.rs:1212-1240`); опция `--shell PATH` (и поле опций сессии в тестах) задаёт программу без изменения окружения процесса. Тесты используют `/bin/sh`.
* Stdout протокола отделён: посторонний вывод приложения уходит в stderr (как RRiter).

## 4. Протокол

### 4.1. Запуск

* Протокол: `ronsole --headless [--script FILE] [--size WxH] [--scale S] [--profile DIR] [--keep-profile] [--shell PATH] [--hz N | --budget-ms F]`. Без `--script` команды читаются из stdin. EOF — как `quit`. Размер по умолчанию — 1280x800, масштаб 1.0. При старте — одна вкладка терминала и `settle 500`. `--hz`/`--budget-ms` — бюджет кадра для `bench`, взаимоисключающие; по умолчанию 240 Гц (монитора без окна нет).
* PGO (Q21): `ronsole --headless --pgo-train --pgo-workspace … --pgo-report … --pgo-timeout-seconds …` (§3.3, §8).
* Разбор: в `main` после `tab-status` (`main.rs:126-128`) и до `parse_startup_args_with_current_dir` (`main.rs:129`) и `acquire_single_instance` (`main.rs:149`): первый аргумент `--headless` — остаток разбирает headless. При `--pgo-train` аргументы, кроме `--headless`, без изменений уходят в `automation_options_from_args` (его отказ на неизвестных, `app/automation.rs:113-116`, остаётся); `--pgo-train` вместе с флагами протокола (`--script`, `--hz`…) — код 2.

### 4.2. Команды

Грамматика, границы и ответы — те же, что в RRiter (`src/headless/protocol.rs:136-268` RRiter, `docs/headless.md:50-82` RRiter): UTF-8, одна команда на строку, пустые строки и строки с `#` в начале пропускаются; ответ — ровно одна строка `ok`, `ok <payload>` или `err <reason>` (причина ≤ 64 символов, переводы строк в payload заменяются пробелом); неизвестная команда — `err unknown command '<name>'`; после `err` следующая команда исполняется. Координаты `mouse_move` — физические пиксели буфера.

| Команда | Ответ | Примечание |
|---|---|---|
| `type <text>` | `ok` | Остаток строки; `\n`, `\t`, `\\` — экранирование, прочее буквально. Уходит одним IME commit (`on_ime_commit`), как в RRiter (`headless/mod.rs:539-542` RRiter); `mods` на него не влияют. |
| `key [--repeat N] [--char C] <combo>` | `ok` | Ниже. N ≤ 1000; неизвестное имя — `err unknown key token '<t>'`. |
| `mods [ctrl] [shift] [alt] [super]` | `ok` | Q16: удерживаемые модификаторы до следующей `mods`; без аргументов — отпустить все. Ниже. |
| `mouse_move x y` | `ok` | Физические пиксели. |
| `mouse_cell <col> <row>` | `ok <x> <y>` | Q20: указатель в центр клетки видимой области активного терминала (§5.1, формула клетки; `x`, `y` — целые пиксели), дальше как `mouse_move`; клетка вне видимой области, центр вне `layout.body` (обрезанная крайняя строка) или кадра ещё не было — `err`. Новая и в RRiter. |
| `click [left\|right\|middle] [down\|up] [alt]` | `ok` | Как RRiter: по умолчанию left, обе фазы; `alt` добавляет Alt к удерживаемым на время клика. |
| `wheel dx dy [lines\|px]` | `ok` | По умолчанию `lines`. |
| `resize WxH` | `ok <w>x<h>` | Физический размер, 320x200…8192x8192; логический = физический / масштаб. Сначала новый pbuffer, затем `on_resize_with_logical_size`; ошибка pbuffer — `err`, состояние прежнее (Q25). |
| `scale S` | `ok` | 0.5…4.0; физический размер сохраняется, логический пересчитывается, `on_scale_changed`. |
| `screenshot <out.png>` | `ok <abs path> <w>x<h>` | PNG текущего кадра (кадр рисуется, если запрошен); каталог создаётся. |
| `dump [out.json]` | `ok <json>` или `ok <abs path>` | §5.1. |
| `dump row <n>` | `ok <json>` | §5.2. Строка ровно из двух токенов `row` и `<uint>` — запрос строки; иначе остаток строки — путь (так же в RRiter). `n` вне видимой области — `err row out of range`. Новая и в RRiter. |
| `settle [ms]`, `wait <ms>`, `wake <ms>`, `idle <ms>` | §3.3 | |
| `bench <frames> [csv=<path>] [action]` | `ok <json>` | Действия `none`, `wheel dx dy`, `key <combo>`, `type <text>`; колонки CSV и ключи summary — как у RRiter (`docs/headless.md`, Bench, RRiter), кроме фаз рендера редактора `root_*`/`chrome_*` (Q29), `hz_source` — `arg\|default`. |
| `info` | `ok <json>` | `gl_renderer`, `gl_version`, `gl_vendor`, `profile`, `budget_ms`, `hz`. |
| `quit` | `ok` | |

Команды RRiter, у которых нет смысла в Ronsole (`open`, `workspace`, `dialog`, `picker_answer`, `record`, `dblclick`), отвечают `err unknown command`. Вкладки открываются горячими клавишами Ronsole (`key ctrl+shift+t`, `app.rs:133-135`) и переключаются кликом по `dump.tabs[].rect`: клавиатурного переключения в Ronsole нет.

**`key`.** Имена клавиш — как в RRiter (`src/keymap.rs:49-88` RRiter: `enter`, `numpadenter`, `tab`, `escape`/`esc`, `backspace`, `delete`, `space`, стрелки, `home`/`end`, `pageup`/`pagedown`, `insert`, `pause`, `f1`–`f12`, пунктуация, `a`–`z`, `0`–`9`), модификаторы `ctrl|control|shift|alt|super|meta`. Таблица headless даёт по имени evdev keycode, `PhysicalKey` (`KeyCode` из `src/input_types.rs:92-156`; клавиши без своего `KeyCode` — `PhysicalKey::Unidentified` с настоящим keycode, как приходит из Wayland), символ US-раскладки и `named` (`named_term_key`). `text` — по тому же правилу, что Wayland (`should_emit_xkb_text`, `src/wayland_input.rs:398-430`: при Ctrl или Super текста нет; Shift — символ верхнего регистра US); `unshifted` — символ US без Shift. Правило текста и keycode модификаторов берутся из Wayland-пути, без копий. `--char C` (Q19) заменяет символ раскладки: `unshifted = C`, текст (если он выдаётся) — `C`, с Shift — `C` в верхнем регистре; так моделируется нелатинская раскладка, как в тесте `key_event_maps_russian_ctrl_c` (`src/input.rs:1561-1570`).

Последовательность `key <mods>+<k>`: для каждого модификатора комбинации, которого нет среди удерживаемых `mods`, в порядке ctrl, shift, alt, super — `KeyInput` нажатия левой клавиши-модификатора (`named = TermKey::Modifier(..)`, keycode 29, 42, 56, 125; `wayland_input.rs:476-488`) и `on_modifiers` с новым набором; затем `Pressed` клавиши, N раз `Repeat`; кадр; `Released` клавиши; затем добавленные модификаторы в обратном порядке — `KeyInput` отпускания и `on_modifiers`; кадр. Удерживаемые `mods` после `key` остаются прежними.

**`mods`.** Разница с текущим набором отправляется теми же событиями клавиш-модификаторов и `on_modifiers`, затем один кадр. Удерживаемые модификаторы действуют на `mouse_move`, `mouse_cell`, `click`, `wheel` и складываются с модификаторами `key` и `click alt`.

### 4.3. Драйвер

`scripts/ronsole_headless.py` — те же подкоманды и флаги, что `rriter_headless.py`: `shot`, `bench`, `run <script|->`, `repl`; общие флаги `--size`, `--scale`, `--profile`, `--keep-profile`, `--shell`, `--hz`, `--budget-ms`; бинарь — `target/x86_64-unknown-linux-gnu/release/ronsole` (переопределение `RONSOLE_BIN`; нет — подсказка `make fast`, код 2); `err` в ответе — код 1; снимки по умолчанию — `/tmp/ronsole-headless`. Подкоманда `shot` без аргумента-цели (в Ronsole нечего открывать) — одна вкладка, `settle`, `screenshot`.

## 5. `dump`

### 5.1. Общий

Компактный JSON в одну строку. Раздел `terminal` снимается за одну пару блокировок в порядке потока разбора PTY — `bridge_screen_lock`, затем `grid` (`src/terminal_process.rs:1068-1070`), — и согласован внутри себя. Ключи:

* `size` `[w, h]`, `scale`, `cursor_icon` — вид указателя из `set_cursor_kind`, имя варианта `CursorKind` через `{:?}` (как RRiter `dump.rs:223`, например `"Pointer"`); соответствие имён двух приложений — в Task 8 плана RRiter.
* `tabs`: `[{"index","title","active","rect"}]` (`App.terminals`, `active_terminal`, `app.rs:379-380`; заголовок — то, что показывает вкладка, `write_display_title`, `terminal.rs:2033`; `rect` `[x,y,w,h]` — тело вкладки в прошлом кадре из `terminal_tab_hitbox_geometry`, `terminal_ui.rs:376-382`, или `null`, если вкладка вне полосы).
* `terminal` — активная вкладка:
  * `cols`, `rows` — размер экрана `Term`;
  * `viewport`: `{"top","scrollback","total"}` — абсолютный индекс (как `Term::row`, `crates/ronsole-term/src/term.rs:276-283`) верхней строки видимой области, длина скроллбэка, всего строк. Видимая область (Q24) — `rows` строк, начиная со строки `top = max(0, total − floor((viewport_h + scroll_offset) / char_h))`, где `viewport_h = body.h − top_pad − bottom_pad` — та же высота, из которой рендер берёт `rows = floor(viewport_h / char_h)` и `max_scroll` (`terminal_ui.rs:1006-1066`), без строк overscan; при прокрутке на целое число строк совпадает с `total − rows − scroll_offset / char_h`, при `scroll_offset = max_scroll` даёт `top = 0`. `scroll_offset` здесь — смещение кадра, округлённое рендером до пикселя (`terminal_render_scroll_offset`), а `max_scroll` дробный, поэтому кадр, пиксельно совпадающий с прокруткой до верха (`scroll_offset > 0` и `scroll_offset + 0.5 ≥ max_scroll`), даёт `top = 0` (строку 0 рендер рисует меньше чем на полпикселя выше верха текста; решение A-1, 25.10); `top` не больше `total − rows`, чтобы окно не выходило за `total` при теле ниже двух строк; при `scroll_offset`, не кратном `char_h`, нижняя строка обрезана телом на долю строки (`mouse_cell` по такой клетке — `err`, если её центр вне `body`). Строка видимой области `v` — абсолютная `top + v`;
  * `text` — массив строк видимой области через `Term::row(top + v)` (как рисует рендер, `terminal_ui.rs:2820`), хвостовые пробелы срезаны; не `Term::visible_row` — он читает экран без прокрутки;
  * `cursor`: `{"row","col"}` в строках видимой области (абсолютная строка курсора — `scrollback + y` из `Term::cursor`, `term.rs:305-307`) или `null`, если курсор вне её; без формы (Q15);
  * `title`;
  * `modes`: `app_cursor_keys` (DECCKM), `bracketed_paste`, `mouse_tracking` (`"none"|"press"|"button_motion"|"any_motion"`, `MouseTrackingMode`), `mouse_sgr`, `focus_reporting` (1004), `kitty_flags` (число), `sync_pending` (есть `sync_deadline()`, режим 2026 держит вывод);
  * `selection`: `{"start":[row,col],"last":[row,col]}` или `null` — `Term::selection` (абсолютные `(col,row,col,row)`, `term.rs:74`), нормализованный `normalized_selection_bounds` (`grid.rs:12-34`); `last` — последняя выделенная клетка включительно; строки — в координатах видимой области, могут быть отрицательными или ≥ её высоты;
  * `hovered_link`: `{"uri","ranges":[[row,col_start,col_end]…]}` или `null` (`terminal.rs:2037`); из `LinkTarget.cells` (абсолютные строки, полуоткрытый `Range`, `crates/ronsole-term/src/links.rs:14-17`) — строки в координатах видимой области, `col_end` исключающий;
  * `layout`: `{"body":[x,y,w,h],"cell0":[x,y],"char_w","char_h","baseline","x_height"}` из `TerminalUiLayout` прошлого кадра (`terminal_ui.rs:158-172`) и `TerminalFontMetrics` (`src/renderer.rs:109-114`), или `null` до первого кадра. `cell0` — левый верхний угол клетки (0, 0) видимой области без округления: `x = text_x`, `y = body.y + body.h − bottom_pad − char_h − (total − 1 − top) · char_h + scroll_offset` (`terminal_ui.rs:1435-1445`); `baseline = baseline_offset · TERMINAL_TEXT_SCALE` (`terminal_ui.rs:2879-2880`); `x_height` — в пикселях. Клетка (c, v): `x0 = round(cell0.x + c·char_w)`, `x1 = round(cell0.x + (c+1)·char_w)` (`terminal_ui.rs:1201-1216`), `y0 = cell0.y + v·char_h`, `row_bottom = floor(y0 + char_h)` (`terminal_ui.rs:2880-2881`), базовая линия `round(y0 + baseline)`. Центр для `mouse_cell` — `(floor((x0 + x1) / 2), floor(y0 + char_h / 2))`.
* `clipboard`: `{"mode":"memory","text":…}`.
* `external_request` (Q22): `{"kind":"open_url","arg":…}` или `null` — последний перехваченный запрос; `dump` его читает и очищает (как `ExternalRequestLog::take` RRiter, `desktop_request.rs:77-83`). Для O2 `arg` — `file://<каталог-родитель>` (`platform/process.rs:396-397`).
* `event_loop`: `control_flow` (`wait|poll|wait_until`), `deadline_ms` (до ближайшего срока или `null`), `wake_pending` (eventfd не прочитан), `redraw_requested`, `frame_requests` (число вызовов `request_frame` с начала сессии), `frames` (нарисовано кадров). Принудительный кадр команды ввода сбрасывает `redraw_requested`, поэтому «команда запросила кадр» проверяется ростом `frame_requests` между двумя `dump`.

Имена и смысл ключей `terminal.*` (включая видимую область и `layout`), `clipboard`, `external_request`, `event_loop.frame_requests` — общий контракт с RRiter (план RRiter добавляет то же для вкладки терминала); расхождение — дефект любой из сторон. Исключение по построению — значение `external_request` для O2: Ronsole `open_url(file://<родитель>)`, RRiter `reveal_path(<файл>)`, и для обычного файла или каталога по `file://` — RRiter `open_path(<путь>)`; сверка их нормализует (Q22). Пиксельные ключи (`layout`, `tabs[].rect`) общие по смыслу, а не по значениям.

### 5.2. `dump row <n>`

`{"row":n,"cells":[{"col","ch","fg","bg","flags","underline","underline_color","link"}…]}` для строки `n` видимой области активной вкладки: `fg`/`bg` — итоговые цвета `#rrggbb` после палитры, inverse и dim, как их считает рендер (`terminal_cell_render_colors`, `terminal_ui.rs:1237`), без подсветки выделения и поиска; `bg` — `null` для фона по умолчанию; `flags` — массив из `bold`, `dim`, `italic`, `inverse`, `strike`, `conceal`, `wide`, `wide_spacer` (`CellFlags`, `HOST_HIDDEN` не выводится); `underline` — `none|single|double|curly|dotted|dashed` (`UnderlineStyle`); `underline_color` — `#rrggbb` или `null`; `link` — URI OSC 8 или `null`. Пустая ячейка без атрибутов пропускается. Снимок — под той же парой блокировок, что §5.1.

## 6. Тестовая обвязка

`src/headless/tests.rs` по образцу RRiter: `session_for_test(w, h) -> HeadlessSession` (профиль во временном каталоге с `process::id()`, шелл `/bin/sh`, в конфиге профиля явный `terminal_background`), `run_script(&mut session, &[u8]) -> Vec<String>`, `wait_until(&mut session, timeout_ms, what, |s| cond)` (крутит `wait 50`, по таймауту паника с `dump`), `native_wake(&mut session, ms)` (то же, что `wake`, плюс время начала шага), `dump(&mut session) -> serde_json::Value`, `cell_rect(&dump, col, row)` (формула §5.1), `pixel_rgb(&image, (x, y)) -> [u8; 3]`. Асинхронный результат ждётся `wait_until`; доставку пробуждений проверяют `wake`/`idle` (Q18); фиксированный `wait` — только для отрицательных проверок и настоящих задержек.

Тесты с GL требуют EGL surfaceless (Mesa). В `Makefile` Ronsole вендора нет: цель `test` получает `TEST_EGL_VENDOR ?= mesa` и передаёт его переменной вендора Ronsole (её читает `prefer_egl_vendor`, `main.rs:40-89`), `session_for_test` вызывает `prefer_egl_vendor`, как RRiter (`headless/mod.rs:257-258` RRiter), — тесты не занимают дискретный GPU. Каждый тест — свой процесс (`-Z panic-abort-tests`, `Makefile:90`), `TEST_THREADS` не меняется. Всё, что тест пишет вне процесса, уникально на процесс.

## 7. Замер памяти

`measure-rss.sh` (git-ignored, `.superpowers/sdd/2026-10-21-terminal-parity-ronsole/`) заменяется скриптом в репозитории `scripts/measure_rss.py` на драйвере: `ronsole --headless`, простой 3 с — `VmRSS` и `smaps_rollup` (`Rss`, `Pss`, `Private_Dirty`) процесса Ronsole, затем заполнение скроллбэка (10 000 строк по 120 колонок; ширина подбирается `resize` по `terminal.layout.char_w` первого `dump`, пока `cols` не станет 120; ожидание последней строки по `dump`) и тот же замер; 3 прогона, медиана. Без `WAYLAND_DISPLAY`. Числа headless и окна различаются (нет wl-буферов) — сравнение «до/после» только headless с headless.

## 8. PGO

* `--headless --pgo-train` — режим Q21 (§3.3, §4.1): `app/automation.rs` (`AutomationStep`, `SemanticBlock`) исполняется на headless-сессии тем же вводом через App, без окна и без протокола.
* `scripts/pgo_pipeline.py`: `resolve_wayland_socket` (`:418-438`) и требование `WAYLAND_DISPLAY` (`:480`) удаляются; команда (`:570-580`) — `ronsole --headless --pgo-train --pgo-workspace … --pgo-report … --pgo-timeout-seconds …`; изоляция `HOME`/`XDG_*` (`:454-486`), родительский таймаут и разбор кода выхода и отчёта (`:164-194`, `:695-709`) — как сейчас.
* После приёмки headless-PGO (Q7): сборка PGO-бинаря и установка в `~/bin/system/ronsole` — последним шагом плана, главной сессией.

## 9. Покрытие L1–L20 (Q4)

Ожидания — из спеки паритета и справочника протокола, не из вывода кода.

### 9.1. Правила сценариев

* Шелл — `/bin/sh`; команды вводятся `type`; длинный сценарий тест пишет файлом во временный каталог процесса и запускает `sh <файл>`.
* Маркер готовности или результата никогда не встречается буквально в набранной команде (эхо команды не даёт ложного успеха): маркер собирается из частей, например `printf 'RE''ADY\n'`.
* Байты, которые терминал отправляет приложению (L2, L8, L9, L10): читатель — `stty raw -echo min 1 time 0`, маркер готовности с `\r\n` (`printf 'RE''ADY\r\n'`, иначе hex начнётся не с первой колонки), затем `{ dd bs=1 count=1; stty min 0 time 10; dd bs=1; } 2>/dev/null | od -An -tx1 -v | tr -d ' \n'`, маркер конца с новой строки (`\r\n`: `raw` выключает OPOST), `stty sane`. Вывод — одна hex-строка без пробелов; длинная переносится терминалом, поэтому тест склеивает строки `dump.text` между маркерами без разделителя. Первый `dd` ждёт первый байт без срока (медленный тест не обрежет ответ), второй читает до 1 с тишины — лишние байты попадают в вывод. Тест ждёт маркер готовности (`wait_until`), шлёт ввод, ждёт маркер конца и сравнивает весь hex-вывод с ожиданием из таблиц спеки паритета целиком, а не префиксом. Отрицательная проверка «байтов не было» (L9): после проверяемого действия фиксированный `wait`, затем `mods` без аргументов (удерживаемый Ctrl превратил бы `x` в `18`) и `key x` — весь вывод читателя равен `78`. `read -t` не используется (не POSIX).
* Пиксели: прямоугольник клетки — `cell_rect` по `terminal.layout` (§5.1); «фон» — `terminal_background` из конфига профиля.
* Шрифты: L11 и L12–L14 рисуются встроенными гранями JetBrains Mono Nerd Font Regular/Bold/Italic/BoldItalic (`src/renderer.rs:236-241`) — от системы не зависят. L17 требует системный `NotoColorEmoji.ttf` (`renderer.rs:364-366`): нет файла — тест падает с сообщением, называющим файл, а не проходит молча.

### 9.2. Сценарии

| L | Возможность | Проверка |
|---|---|---|
| L1 | SGR: bold, italic, dim, inverse, conceal, strike, подчёркивания 4:1–4:5, 21, цвет 58/59 | `dump row`: `flags`, `underline`, `underline_color`, `fg`/`bg` после inverse/dim; conceal — пиксели клетки цвета фона. |
| L2 | DA2, XTVERSION, DECRQM, OSC 10/11/12/4 с `?` | Читатель §9.1 печатает ответ терминала — `dump.text`. |
| L3 | 2026: буфер и срок 150 мс без новых байтов | `printf '\033[?2026h'; printf 'vis''ible\n'; sleep 1`. Отсчёт — от первого `dump` с `sync_pending` (`wait_until`): в нём маркера нет; маркер виден не позже 150 мс + 100 мс допуска от этого `dump`; отдельно `idle 400` после начала буфера — `deadlines ≥ 1` (срок 2026 будит цикл). |
| L4 | OSC 8 переживает скроллбэк и reflow | Ссылка, затем 2·`rows` строк, `wheel` вверх до строки ссылки — `dump row` `link`; затем `resize` — та же проверка. |
| L5 | Обычные URL (http/https/file) | `mods ctrl` + `mouse_cell` на URL → `hovered_link`; `click` → `external_request` `open_url` с URL. |
| L6 | OSC 52 | `dump.clipboard.text`. |
| L7 | OSC 7: новая вкладка в папке активной | OSC 7 на временный каталог, `key ctrl+shift+t`, `pwd` в новой → `dump.text`. |
| L8 | Kitty (флаги, стеки, release/repeat), Shift+Enter, Ctrl по физической клавише, Ctrl+C, IME текстом | Читатель §9.1, ожидание из таблиц спеки 5.x; Ctrl по физической клавише — `key --char с ctrl+c` → `03`; IME — `type` с не-ASCII текстом. AltGr Windows — вне Linux-headless. |
| L9 | Ctrl-наведение: pointer + подчёркивание; Ctrl+клик не уходит в приложение при tracking | `cursor_icon`, пиксель подчёркивания в `cell_rect`; при `ESC[?1000h` и читателе §9.1 `mods ctrl` + `click` → `external_request`, читатель не получил байтов. |
| L10 | Мышь при tracking, 1004, bracketed paste | Отчёт мыши: `mouse_cell c r` + `click` → SGR с клеткой `c+1;r+1`. 1004 (тест только Ronsole, Q26): читатель во вкладке 1, `key ctrl+shift+t` (вкладка 1 теряет фокус — `ESC[O`), клик по `dump.tabs[0].rect` (`ESC[I`) — вывод читателя в `dump.text` вкладки 1. Paste: выделение протягиванием (`mouse_cell` + `click left down` → `mouse_cell` → `click left up`), `key ctrl+c`, читатель с `ESC[?2004h`, `key ctrl+v` → `1b 5b 32 30 30 7e … 1b 5b 32 30 31 7e`. |
| L11 | Начертания Bold/Italic/BoldItalic, фолбэк в Regular | Пиксели: глиф bold шире/темнее regular той же буквы; символ без грани начертания рисуется. |
| L12 | Слой оформления, зачёркивание по x-height, цвет после inverse/dim | Верх подчёркивания не выше базовой линии клетки (`layout.baseline`); цвет подчёркивания — `underline_color` из `dump row`, если задан (SGR 58), иначе `fg`; цвет зачёркивания — `fg`. |
| L13 | Подчёркивание не выходит за строку | Пикселей подчёркивания нет при `y ≥ row_bottom` (строка ниже пустая) при масштабах 1.0 и 1.333. |
| L14 | Зачёркивание центрировано | Центр полосы — в пределах ±1 px от середины чернил глифа `x` в той же строке (измерение по PNG, не по метрикам кода). |
| L15 | Смена наведённой ссылки запрашивает кадр (в т.ч. Ctrl без движения) | Указатель над ссылкой без Ctrl, `dump`; `mods ctrl` без `mouse_move` → `frame_requests` вырос, `hovered_link` не `null`. |
| L16 | Диапазон ссылки через `partition_point` | Ссылка на две строки и два отрезка в строке → `hovered_link.ranges`. |
| L17 | Цветные/неотображаемые символы делят запись Regular | Эмодзи в bold и regular рисуются одинаково (пиксели). |
| L18 | Замер памяти | Не сценарий: §7. |
| L19 | Ручная проверка | Не сценарий. |
| L20 | O2: `file://` не запускает исполняемые и `.desktop` | Ctrl+клик по ссылке на исполняемый файл во временном каталоге → `external_request.arg` = `file://<каталог>` (ждётся `wait_until`). |

Отдельный тест пробуждения (Q18): шелл печатает строку через `sleep 1`; `idle 2000` → `wakes ≥ 1`, строка в `dump.text` появилась до конца `idle`.

## 10. Ошибки

* Нет EGL — код 3 и строка `headless: EGL setup failed at <stage>: <detail>` (§3.2); в режиме `--pgo-train` ещё и отчёт о сбое старта.
* Неверные аргументы — код 2.
* Ошибка разбора или исполнения команды — `err <reason>`, сессия продолжается; код выхода 1.
* Шелл не запустился — сообщение в сетке, как в окне; `dump.text` его показывает.
* Падение потока PTY не роняет сессию; `wait_until` по таймауту печатает `dump`.

## 11. Приёмка

1. `make test` проходит без Wayland-сессии (`env -u WAYLAND_DISPLAY`), headless-сценарии §9 зелёные.
2. `scripts/pgo_pipeline.py` тренирует без `WAYLAND_DISPLAY`, окна на экране не появляется.
3. `scripts/measure_rss.py` даёт медианы трёх прогонов без окна.
4. Общее тело сценария `scripts/term_parity/` RRiter (команды §4.2, координаты мыши — `mouse_cell`, без `dump.tabs[].rect`, Q26) после пролога Ronsole исполняется `ronsole_headless.py run` без правок тела. Предусловие — в плане RRiter (Task 8 или шаг перед ним): команды `mods`, `mouse_cell`, `dump row`, `key --char`; ключи §5.1 (`terminal.viewport`, `terminal.layout`, координаты `selection`/`hovered_link`/`cursor` в видимой области, `event_loop.frame_requests`, `external_request` `{"kind","arg"}`); нормализация O2 в сверке (Q22); форма курсора не сравнивается (Q15); часть L10 про 1004 и любой клик по вкладке — не в общем теле (Q26).
5. Обычный запуск Ronsole в окне не изменился (ручная проверка пользователем и существующие тесты).

## 12. Этапы (Q28)

**Этап 1 — PGO без окна.** §3.1 целиком (шов, все точки таблицы, порядок уничтожения, `resize` по Q25); §3.2 целиком; §3.3 — функция шага, общая с Wayland-циклом, темп кадра и пробуждение по сроку темпа, выход App, режим `--pgo-train` (Q21, Q25, Q27); §3.4 — профиль, обход single-instance (Q17), записывающие буфер обмена и открытие ссылок (Q27); шелл — по умолчанию, без `--shell`; §4.1 — только разбор `--headless --pgo-train` (`--headless` без `--pgo-train` на этапе 1 — код 2 с сообщением «протокол не реализован»); §8 целиком; §10 — коды и сообщения для `--pgo-train`; из §6 — `TEST_EGL_VENDOR` в `Makefile` и вызов `prefer_egl_vendor` в тестовой сессии. Тесты этапа 1 — Rust-тесты без протокола: headless-сессия создаётся и рисует кадр, `resize` меняет pbuffer, короткая автоматизация доходит до `ok`, ошибка сессии даёт отчёт `failed`. Приёмка — §11.2, §11.5, `make test` зелёный; затем сборка PGO-бинаря и установка в `~/bin/system/ronsole` (Q7) главной сессией.

**Этап 2 — протокол и проверки.** Остальное: режим протокола и команды §3.3 (`settle`, `wait`, `wake`, `idle`), §4.1–§4.3, §5, остаток §6, §7, §9, §11.1, §11.3, §11.4. Этап 2 — до исполнения плана паритета RRiter (Q3).
