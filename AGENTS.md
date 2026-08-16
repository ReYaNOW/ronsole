# Ronsole agent rules

Ronsole is a Linux-only, Wayland-only terminal emulator extracted from RRiter. RRiter is the read-only donor/source-of-truth. Keep Ronsole standalone and do not reintroduce editor, Git, LSP, database, project/workspace, HTTP, X11, Windows, or macOS code.

## Priorities

1. Smooth high-refresh rendering and input latency.
2. Low idle CPU/GPU use.
3. Low RAM/VRAM use with bounded caches and buffers.
4. Small, maintainable modules with one implementation per behavior.
5. Pixel-stable rendering at fractional scale factors.

## Hot-path rules

- No filesystem I/O, `/proc` scanning, clipboard access, subprocess work, or configuration loading in render paths.
- No recurring large `Vec`/`String` allocation per frame. Keep scratch buffers on long-lived app/renderer state and reuse them with `clear()`.
- Keep renderer vertex buffers, glyph atlases, and glyph caches persistent and bounded.
- Do not `format!()` tab/search/status strings every frame; cache formatted text when source state changes.
- Do not take the same mutex repeatedly in one frame when a single bounded lock can cover the required snapshot.
- Render only when state is dirty or animation is active. Settled idle state must sleep in `ControlFlow::Wait`/`WaitUntil`.
- Unfocused, occluded, and zero-size windows must not continuously render.
- Use real frame `dt`; do not quantize to 60 Hz or add sleep-based FPS limiters.
- Round UI geometry/baselines consistently before drawing. Text, cursor, selection, and hitboxes sharing a row must use the same snapped geometry.

## Platform/runtime invariants

- Linux + Wayland only. Do not enable `winit/x11`, GLX, or X11 features.
- EGL vendor selection must happen before EGL/GLVND loads.
- GL context priority order on Linux: High, then Default fallback for each supported context plan.
- Context plans: OpenGL 4.1 Core, OpenGL 3.3 Core, GLES 3.0.
- Default framebuffer request: transparent=false, depth=0, stencil=0. Prefer hardware acceleration, then fewer samples.
- Present with `SwapInterval::Wait(1)` and no second frame limiter.
- Keep diagnostics opt-in and out of frame loops.

## Renderer ownership

- `src/renderer.rs`: GL resources, batching, atlases, font/glyph caches, scissor/clip, text measurement, primitive drawing, pixel snapping.
- `src/runtime.rs`: Wayland window + glutin context/surface lifecycle and graphics diagnostics.
- `src/app.rs`: winit event routing, redraw/idle/focus/occlusion/resize lifecycle.
- `src/main.rs`: Linux startup, EGL vendor preference, allocator tuning, event-loop construction.
- Later terminal/session/input/tab modules must reuse these layers rather than create parallel GL/window/render implementations.

## Memory/resource rules

- Scrollback, PTY queues, line pools, glyph caches, atlases, and search results must be bounded.
- Prefer mmap/shared system font data over eager heap copies where practical.
- Do not create hidden full-window FBOs or MSAA buffers unless a measured feature requires them.
- Avoid duplicate glyph/color atlases and unrelated icon/font assets.
- No `#[allow(dead_code)]` to hide transitional code. Remove unused code/dependencies created by the current task.

## Tests and verification

Each task must add focused regressions for its behavior. Keep tests pure where OS/GL runtime is not required.

Long-running commands including unpack, compilation, `cargo check`, `cargo test`, and `cargo clippy` must start through `nohup`; poll completion with separate short commands.

Required before delivery when applicable:

- `cargo fmt --all -- --check`
- `cargo check --all-targets`
- `cargo test --all-targets`
- `cargo clippy --all-targets -- -D warnings` when clippy is available
- `git diff --check`
- conflict-marker scan

Never run `cargo clean`. Never commit or push from agent work.

## Terminal backend ownership

- `src/terminal.rs` owns the terminal parser/grid state and the small session facade. Keep rendering, tabs, search, and settings out of this module.
- `src/terminal_compat.rs` owns compact terminal cell/color representation plus Unicode width/combining and SGR color helpers. Keep PTY/process/render ownership out of this module.
- `src/terminal_process.rs` owns PTY spawn/I/O batching, shell startup, cached dynamic titles, and bounded shutdown.
- `src/platform/process.rs` is the Linux-only `/proc` process/session helper. Dynamic-title discovery must stay bounded to the terminal foreground process group; full `/proc` enumeration is allowed only in terminal shutdown/cleanup to reap every process group that remains in the owned PTY session. No `/proc` work may run from the render path.
- Every session starts in `$HOME`; do not reintroduce workspace/current-process CWD heuristics.
- PTY output must stay batched and bounded. One parser/grid lock and one redraw wakeup per output batch is the target invariant.

## Terminal interaction ownership

- `src/renderer/terminal_ui.rs` owns terminal-body/search-overlay geometry and drawing. It may inspect only the visible grid rows plus bounded overscan and must not perform clipboard, filesystem, `/proc`, or other system I/O while the grid mutex is held.
- `src/input.rs` owns keyboard, mouse, selection, terminal protocol input, and lazy Wayland clipboard routing for the active session. Clipboard access is event-driven only.
- `src/search.rs` owns terminal search state and cached match computation. Re-scan only when the query options or terminal content generation changes; rendering must consume precomputed row matches.
- `src/scroll.rs` is the shared smooth scroll/scrollbar implementation. Terminal scrolling uses its existing target/current interpolation rather than a second animation model.
- `src/tabs.rs` owns shared terminal-tab ordering, drag-and-drop placement/render ordering, reveal targeting, and edge-autoscroll math. Keep tab session lifecycle and shortcut dispatch in `src/app.rs`; do not duplicate these algorithms in renderer/input code.
