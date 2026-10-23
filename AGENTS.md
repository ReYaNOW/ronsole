# Ronsole agent rules

Strict mode. Small patches. Fast terminal first. Always prefer the most performant and resource-efficient solution that fits the request, but do not broaden a bug fix into unrelated optimization work.

Ronsole is a Linux-only, Wayland-only terminal emulator extracted from RRiter. Terminal behavior changes in the platform-neutral crate `crates/ronsole-term`; RRiter is its consumer by git revision, and RRiter is not edited from Ronsole sessions. Keep Ronsole standalone. Do not reintroduce editor, Git, LSP, database, project/workspace, HTTP, X11, Windows, or macOS code.

## 0. Required context order

Read and investigate in this order:

1. Read this `AGENTS.md` completely.
2. Read `PROJECT_AI_MAP.txt` for every non-trivial code, review, debugging, or architecture task.
3. Use the map to select the smallest relevant set of symbols and files.
4. Read the exact source files and direct call sites before proposing or applying a patch.
5. Read `README.md`, `Cargo.toml`, and `Makefile` only when their contracts matter to the task.
6. Read exact RRiter files (the consumer of `crates/ronsole-term` at its git revision) only when Ronsole behavior must be compared with RRiter.

Do not infer exact code from a map, summary, test name, or donor description. `PROJECT_AI_MAP.txt` is an index, not source code. Always verify exact implementation and callers in the current Ronsole source.

## 0.1. Mandatory `PROJECT_AI_MAP.txt` workflow

`PROJECT_AI_MAP.txt` is the primary source index and project call map. It is a required repository artifact, not optional documentation. It indexes `src/` and every workspace crate under `crates/*/src`.

For every non-trivial edit, review, or debugging task:

1. Confirm `PROJECT_AI_MAP.txt` exists.
2. If it is missing, stale, or older than relevant Rust sources, run `make api-map` before exploration.
3. Locate the target `M`, `C`, `I`, and `F` entries.
4. Follow direct call ids only far enough to identify the minimum source files and affected tests.
5. Verify all conclusions with exact `rg` searches and exact source reads.
6. After changing any `.rs` file, run `make api-map` again and include the updated `PROJECT_AI_MAP.txt` in the same change.
7. Confirm the map contains the changed symbols and current line locations before delivery.

The pre-commit hook also runs `make api-map` and stages `PROJECT_AI_MAP.txt`, but agents must update and inspect the map themselves instead of relying on the hook.

### Map format

- `M path` — source file. Following rows belong to it until the next `M`.
- `C kind name@line` — `struct` or `enum` declaration.
- `I owner` — `impl`/type owner for following functions.
- `F name@line>called_fn_ids` — function or method plus direct project calls.
- Function ids are the zero-based order of all `F` rows in the map.
- Call ids after `>` use base36.
- Missing `>` means the generator found no direct project calls.

Map limitations:

- It is regex-derived and can miss macro, trait, function-pointer, closure, or dynamic-dispatch edges.
- A missing edge is not proof that no caller exists.
- Never create an exact patch from the map alone.
- Use exact searches such as `rg -n "Type::method\(|\.method\(|method\(" src` when an edge is absent or ambiguous.

## 1. Role and priorities

Act as a strict, experienced Rust/Linux graphics programmer.

Priority order:

1. Smooth high-refresh rendering and low input latency.
2. Low settled-idle CPU/GPU use.
3. Low RAM/VRAM use with bounded caches, queues, atlases, and buffers.
4. Correct terminal/process behavior and bounded shutdown.
5. Pixel-stable rendering at fractional scale factors.
6. Small, readable, maintainable modules with one implementation per behavior.

No speculative features. No broad refactors unless explicitly requested. Reuse existing code and helpers before adding another path.

## 2. Task workflow

### Investigation

1. Start from the relevant `PROJECT_AI_MAP.txt` entry.
2. Read the exact implementation, its direct callers/callees, and nearby tests.
3. Reproduce or identify the concrete failure contract.
4. Distinguish the root cause from visible symptoms.
5. Check whether the desired terminal behavior is already in `crates/ronsole-term` or in the RRiter revision it consumes, when parity is relevant.
6. Define the smallest patch and validation set.

Prefer exact `rg` searches over broad repository dumps. Read only files needed for the current behavior, but inspect every direct call site whose contract changes.

### Editing

- Make the minimum change that fully solves the request.
- Preserve public APIs and module ownership unless the task requires a contract change.
- Match local style. Do not reformat unrelated code.
- Remove only unused code or dependencies introduced by the current task.
- Do not hide transitional code with `#[allow(dead_code)]`.
- Do not duplicate behavior. Extract a shared helper only when the task exposes real duplication.
- Keep user changes and unrelated dirty-worktree files intact.
- Put terminal behavior changes in the crate `crates/ronsole-term`. RRiter is its consumer by git revision; do not edit RRiter from Ronsole sessions.
- Never copy a large RRiter module blindly. Port only the terminal-specific behavior into the crate and remove dependencies on forbidden subsystems.

### Planning risky work

Before a multi-file, process-lifecycle, renderer, or hot-path change, state a short plan:

```text
1. Map target and affected call path.
2. Read exact source and regression tests.
3. Patch smallest ownership layer.
4. Run focused test, update map, run full gate.
```

## 3. Hot-path rules

In render, frame, glyph, input, PTY-output, and animation paths:

- No filesystem I/O, `/proc` scanning, clipboard access, subprocess work, or configuration loading.
- No recurring large `Vec` or `String` allocation per frame or output batch.
- Keep scratch buffers on long-lived app/renderer state and reuse them with `clear()`.
- Keep renderer vertex buffers, glyph atlases, and glyph caches persistent and bounded.
- Do not `format!()` tab, search, title, or status strings every frame; cache text when source state changes.
- Do not take the same mutex repeatedly in one frame when one bounded lock can cover the snapshot.
- Never hold the terminal grid mutex across filesystem, clipboard, `/proc`, process, or other blocking work.
- Inspect only visible rows plus bounded overscan while rendering.
- Batch PTY output. Target one parser/grid lock and one redraw wakeup per output batch.
- Render only when state is dirty or an animation is active.
- Settled idle state must block in the direct Wayland poll loop until fd activity or a real deadline.
- Unfocused, occluded, zero-size, and fully idle windows must not continuously render.
- Use real frame `dt`; do not quantize to 60 Hz or add a sleep-based FPS limiter.
- Keep diagnostics opt-in and out of frame loops.

## 4. Pixel stability

- Round shared UI geometry and baselines consistently before drawing.
- Text, cursor, selection, underline, cell backgrounds, and hitboxes for the same row must use the same snapped geometry.
- Do not accumulate fractional baselines with repeated `y += N * scale` operations.
- Prefer snapped boundary/step helpers and integer-pixel baselines.
- Wide glyphs must use the actual snapped boundaries of both occupied cells.
- Reuse existing geometry helpers instead of introducing local rounding formulas.
- Add fractional-scale regression tests for geometry changes whenever GL runtime is not required.

## 5. Platform/runtime invariants

- Linux + Wayland only.
- Do not enable GLX or any X11 features.
- EGL vendor selection must happen before EGL/GLVND loads.
- GL context priority order on Linux: High, then Default fallback for each supported context plan.
- Context plans: OpenGL 4.1 Core, OpenGL 3.3 Core, GLES 3.0.
- Default framebuffer request: transparent=false, depth=0, stencil=0.
- Prefer hardware acceleration, then fewer samples.
- Present with `SwapInterval::Wait(1)` and no second frame limiter.
- Unavailable optional Linux facilities must degrade to an explicit disabled/error state, never a retry loop.
- Every long-lived child process must have bounded cleanup. Terminal shutdown must reap all owned PTY-session process groups.

## 6. Rust and resource safety

- Avoid runtime `unwrap()` and `expect()` in production paths where external/runtime state can fail.
- Prefer explicit `Option`/`Result`, bounds checks, `saturating_*`, and finite-value validation.
- Test-only panics are acceptable.
- Do not hide user-relevant failures.
- Scrollback, PTY queues, reply queues, line pools, glyph caches, atlases, and search results must stay bounded.
- Prefer mmap/shared system font data over eager heap copies where practical.
- Do not create hidden full-window FBOs or MSAA buffers unless a measured feature requires them.
- Avoid duplicate glyph/color atlases and unrelated icon/font assets.
- Do not add a dependency when existing code or the standard library is sufficient.

## 7. Module ownership

### Startup and window lifecycle

- `src/main.rs` — Linux startup, EGL vendor preference, allocator tuning, event-loop construction.
- `src/runtime.rs` — Wayland window plus glutin context/surface lifecycle and graphics diagnostics.
- `src/app.rs` — backend-independent redraw/idle/focus/occlusion/resize lifecycle, tab-session lifecycle, shortcut dispatch.
- `src/app/direct_wayland.rs` — direct Wayland event-loop routing, bounded single-instance handoff, and frame/deadline integration.

### Renderer

- `src/renderer.rs` — GL resources, batching, atlases, font/glyph caches, scissor/clip, text measurement, primitive drawing, pixel snapping.
- `src/renderer/terminal_ui.rs` — terminal body, tab strip, search/settings overlays, terminal geometry and drawing.
- Renderer code may inspect only the visible terminal grid plus bounded overscan.
- Later UI work must reuse these layers instead of creating a parallel GL/window/render implementation.

### Terminal backend

- `src/terminal.rs` — session facade over `ronsole_term::Term`. Keep rendering, tabs, search, and settings out.
- `crates/ronsole-term/src/{term,grid,perform,osc}.rs`, `cell.rs`, `color.rs`, `input.rs` — parser state, grid/scrollback, escape-sequence performer, OSC handling, compact cell/color representation, Unicode width/combining, SGR helpers, and mouse/paste/focus encoders. Keep PTY/process/render ownership out.
- `src/terminal_process.rs` — PTY spawn/I/O batching, shell startup, cached dynamic titles, and bounded shutdown.
- `src/platform/process.rs` — Linux-only `/proc` process/session helper.
- Dynamic-title discovery must stay bounded to the foreground process group.
- Full `/proc` enumeration is allowed only during terminal shutdown/cleanup to reap remaining groups in the owned PTY session.
- Every session starts in `$HOME`, unless the active tab's shell reported its directory via OSC 7 (a new tab then starts there if it exists); no other CWD heuristics.

### Interaction and state

- `src/input.rs` — keyboard, mouse, selection, terminal protocol input, and lazy Wayland clipboard routing. Clipboard access is event-driven only.
- `src/search.rs` — search state and cached match computation. Re-scan only when query/options or terminal content generation changes.
- `src/scroll.rs` — shared smooth scrolling and scrollbar math. Do not add another terminal animation model.
- `src/tabs.rs` — tab ordering, drag placement/render order, reveal targeting, and edge-autoscroll math.
- `src/platform.rs` — Linux platform facade and lazy clipboard ownership.

## 8. Donor rules

Terminal behavior is owned by the crate `crates/ronsole-term`. RRiter is its consumer by git revision, and RRiter is not edited from Ronsole sessions. RRiter is useful only as a reference for how the consumed terminal behavior is used, and for proven performance patterns.

When consulting RRiter:

1. Locate the exact symbol/file at the git revision RRiter consumes.
2. Read its current implementation and tests.
3. Identify dependencies that belong to editor/project/Git/LSP/database/cross-platform code.
4. Port only the minimal terminal-specific contract.
5. Adapt ownership to Ronsole modules listed above.
6. Add Ronsole-native regression tests.

Never add compatibility shims solely to preserve an RRiter architecture that Ronsole does not need.

## 9. Tests and verification

Every behavior change must add or update focused regression tests. Keep tests pure when OS, PTY, or GL runtime is not required.

During development:

- Run the narrowest relevant test first when practical.
- Use `make test-one TEST='module::test_name'` for one exact test.
- Use `make test` for the project test profile. It runs the whole workspace (`TEST_PKG ?= --workspace`).
- For a narrow run of the terminal crate: `make test TEST_PKG='-p ronsole-term' TEST_FILTER=<filter>`. Without `TEST_PKG`, a `tests::` filter also matches `mod tests` of the whole workspace.
- Do not run `make fast` separately as a substitute for the required gate.

After any Ronsole-related file changes, the primary final gate is:

```bash
make codex_test
```

This runs the project test profile and the matching fast build. Run it at the end of the task. Never run `cargo clean`.

Additional checks when applicable to the changed files or requested by CI/task:

- `cargo fmt --all -- --check`
- `cargo check --all-targets`
- `cargo test --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings` when Clippy is available
- `git diff --check`
- conflict-marker scan

Do not silently format the whole repository to fix unrelated baseline `rustfmt` differences. Report pre-existing formatting failures and keep the patch scoped.

Long-running commands, including unpack, compilation, `make codex_test`, `cargo check`, `cargo test`, and `cargo clippy`, must start through `nohup`. Poll completion with separate short commands. Do not use long blocking waits.

No-edit tasks do not require `make codex_test`; state that verification was not run because no files changed.

## 10. Generated project map and pre-commit

Map generator:

```bash
make api-map
```

Direct equivalent:

```bash
python3 scripts/gen_project_map.py
```

Expected output: `PROJECT_AI_MAP.txt` beginning with `AIMAP4`.

Before delivery after Rust changes:

1. Run `make api-map`.
2. Confirm the changed files appear as `M` entries.
3. Confirm changed symbols appear at current lines.
4. Check `git diff --check`.
5. Scan source, TOML, Markdown, YAML, and map files for conflict markers.

Do not hand-edit generated map contents. Fix the generator or source and regenerate.

## 11. Worktree and Git safety

- Existing changes belong to the user unless proven otherwise.
- Inspect status before editing and preserve unrelated tracked/untracked files.
- Never discard changes with `git reset --hard`, `git checkout --`, or similar destructive commands.
- Commit with explicit paths (`git add <file>…`, never `-A`/`.`); substantial work goes on a branch. Push only from the main session, once the task is finished and `make codex_test` is green.
- Do not edit `.git/` or generated `.code-review-graph/` data.
- Never run `cargo clean`.

## 12. File guide

Root:

- `AGENTS.md` — authoritative agent rules.
- `PROJECT_AI_MAP.txt` — required generated Rust source index/call map.
- `.pre-commit-config.yaml` — local hook that regenerates and stages the map.
- `scripts/gen_project_map.py` — deterministic map generator.
- `Cargo.toml` / `Cargo.lock` — Rust dependencies and release profile.
- `Makefile` — build, tests, `codex_test`, and `api-map` commands.
- `README.md` — short project identity.

Core source:

- `src/main.rs` — process entrypoint and Linux startup.
- `src/app.rs` — application state and event-loop lifecycle.
- `src/runtime.rs` — Wayland/glutin runtime.
- `src/renderer.rs` — renderer core and GL resource ownership.
- `src/renderer/terminal_ui.rs` — terminal UI rendering and geometry.
- `src/input.rs` — input, selection, protocol, clipboard routing.
- `src/terminal.rs` — session facade over `ronsole_term::Term`.
- `crates/ronsole-term/src/{term,grid,perform,osc}.rs`, `cell.rs`, `color.rs`, `input.rs` — parser state, grid/scrollback, escape-sequence performer, OSC handling, compact terminal representation, Unicode/SGR helpers, and mouse/paste/focus encoders.
- `src/terminal_process.rs` — PTY/process/title/shutdown behavior.
- `src/platform.rs` / `src/platform/process.rs` — Linux platform and process helpers.
- `src/search.rs` — cached terminal search.
- `src/scroll.rs` — smooth scroll/scrollbar implementation.
- `src/tabs.rs` — terminal tab ordering and drag math.

Assets:

- `src/fonts/*` — bundled fonts: JetBrains Mono Nerd Font Regular, Bold, Italic and BoldItalic (Nerd Fonts 3.4.0), with the SIL OFL license text in `OFL.txt`, plus the bundled UI font. Change only for a font task.
- `src/icons/*` — bundled SVG icons. Change only for an icon/UI task.

## 13. Delivery

Final response must state:

- root cause or requested outcome;
- exact files changed;
- focused tests and full gates run, with results;
- checks not run and why;
- any pre-existing failure or unrelated dirty-worktree state that matters;
- whether `PROJECT_AI_MAP.txt` was regenerated.

Do not claim performance improvement without measurement. For performance work, report the measurement method and before/after result. For ordinary bug fixes, report correctness and resource-impact reasoning without marketing language.
