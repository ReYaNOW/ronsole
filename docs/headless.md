# Headless mode

`ronsole --headless` runs the real terminal app without a window: the same `App`,
the same renderer and the same frame code, drawing into an offscreen EGL pbuffer.
Commands arrive on stdin (or from a script) one per line; every command gets
exactly one reply line. Use it to check UI changes, dump terminal state, and
measure frame cost. Linux only. The headless run never touches the
single-instance socket, the system clipboard, `xdg-open`, or the user config.

The design, dump keys and exit-code rationale are in
`docs/superpowers/specs/2026-10-23-ronsole-headless-design.md` (§3–§5).

## Quick start

```sh
make fast                                                   # the driver never builds
python3 scripts/ronsole_headless.py shot                    # prints the PNG path
python3 scripts/ronsole_headless.py shot --out /tmp/check.png --scale 2
python3 scripts/ronsole_headless.py run scripts/headless_selftest.txt --shell /bin/sh
python3 scripts/ronsole_headless.py bench 480 wheel 0 -3    # summary JSON, then CSV path
python3 scripts/ronsole_headless.py repl                    # type commands by hand
```

## Driver

`scripts/ronsole_headless.py` runs the release binary
`target/x86_64-unknown-linux-gnu/release/ronsole` (`RONSOLE_BIN` overrides the
path). Without a binary it prints `ronsole binary not found at <path>; run make fast`
and exits with 2.

| Subcommand | Action | Stdout |
|---|---|---|
| `shot [--out PNG] [--settle MS]` | One terminal tab, `settle` (default 500), `screenshot`, `quit`. Default PNG: `/tmp/ronsole-headless/ronsole-<ts>-<pid>.png`. | The PNG path. |
| `bench FRAMES [action…] [--settle MS]` | `settle`, `bench`, `quit`. | The JSON summary, then the CSV path. |
| `run SCRIPT` | Relays the script to the binary (`--script`). `run -` reads commands from stdin. | Replies on stdout, app output on stderr. |
| `repl` | Relays the caller's stdin. | Replies on stdout. |

Shared flags go after the subcommand: `--size WxH`, `--scale S`, `--profile DIR`,
`--keep-profile`, `--shell PATH`, `--hz N`, `--budget-ms F`. Each is passed to the
binary unchanged; the binary validates them.

Driver exit codes: `0` every reply was `ok`; `1` an `err` reply or unreadable
input; `2` bad arguments or no binary; `3` EGL failure (passed through from the
binary). The app's stderr is inherited unchanged.

Self-test of the protocol (needs a binary):

```sh
env -u WAYLAND_DISPLAY python3 scripts/ronsole_headless.py run scripts/headless_selftest.txt --shell /bin/sh
```

Every shared command appears once in that script and must answer `ok`.
`python3 scripts/ronsole_headless.py --self-test` checks the driver itself without a binary.

## Binary CLI

```
ronsole --headless [--script FILE] [--size WxH] [--scale S] [--profile DIR | --keep-profile]
                   [--shell PATH] [--hz N | --budget-ms F]
```

| Option | Meaning |
|---|---|
| `--script FILE` | Read commands from FILE. Without it, commands come from stdin; EOF is `quit`. |
| `--size WxH` | Framebuffer size, default `1280x800`. |
| `--scale S` | UI scale, default `1.0`. |
| `--profile DIR` | Reusable profile root. Not with `--keep-profile`. |
| `--keep-profile` | Keep the temporary profile on exit. |
| `--shell PATH` | Shell started in terminal tabs. |
| `--hz N` / `--budget-ms F` | Frame budget for `bench`; mutually exclusive. Default 240 Hz (no monitor exists). |

At start the session has one terminal tab and `settle 500` has run.

Exit codes: `0` every reply `ok`; `1` an `err` reply or unreadable script; `2` bad
arguments; `3` EGL setup failed (stderr line `headless: EGL setup failed at <stage>: <detail>`).

## Protocol

UTF-8, one command per line. Empty lines and lines whose first character is `#`
are skipped. A reply is one line: `ok`, `ok <payload>` or `err <reason>`
(reason ≤ 64 characters, line breaks in payloads become spaces). An unknown
command replies `err unknown command '<name>'`. After an `err` the next command
still runs. Coordinates of `mouse_move` are physical framebuffer pixels.

| Command | Reply | Notes |
|---|---|---|
| `type <text>` | `ok` | Rest of the line; `\n`, `\t`, `\\` are escapes. Sent as one IME commit. |
| `key [--repeat N] [--char C] <combo>` | `ok` | `N` ≤ 1000. `--char C` replaces the layout symbol of the key. Modifiers `ctrl`, `shift`, `alt`, `super`. |
| `mods [ctrl] [shift] [alt] [super]` | `ok` | Held modifiers until the next `mods`; no arguments releases all. |
| `mouse_move x y` | `ok` | Physical pixels. |
| `mouse_cell col row` | `ok <x> <y>` | Pointer to the centre of a cell of the visible area. `err` if the cell is outside the visible area, its centre is outside `layout.body`, or no frame has been drawn yet. |
| `click [left\|right\|middle] [down\|up] [alt]` | `ok` | Default `left`, both phases. |
| `wheel dx dy [lines\|px]` | `ok` | Default `lines`. |
| `resize WxH` | `ok <w>x<h>` | 320x200 … 8192x8192, physical size. On failure the size stays. |
| `scale S` | `ok` | 0.5 … 4.0. |
| `screenshot <out.png>` | `ok <abs path> <w>x<h>` | Draws a frame if one is requested; creates the directory. |
| `dump [out.json]` | `ok <json>` or `ok <abs path>` | Terminal and UI state, spec §5. |
| `dump row <n>` | `ok <json>` | One visible row, spec §5.2. `err row out of range` outside the visible area. |
| `settle [ms]` | `ok frames=<n> settled=<bool>` | Default 500. |
| `wait <ms>` | `ok` | Real time, ≤ 60000. Use only for negative checks. |
| `wake <ms>`, `idle <ms>` | `ok` | Loop-model timing, spec §3.3. |
| `bench <frames> [csv=<path>] [none\|wheel dx dy\|key <combo>\|type <text>]` | `ok <json>` | Summary JSON with `csv`. |
| `info` | `ok <json>` | GL strings, profile, budget, `hz`. |
| `quit` | `ok` | EOF does the same. |

Keys use the names of RRiter's key table (`enter`, `tab`, `escape`/`esc`, arrows,
`f1`–`f12`, letters, digits, punctuation). `key` sends press, repeats and release,
and adds modifiers that are not held by `mods`.

Practical notes:

* Tabs: keyboard switching does not exist. Open one with `key ctrl+shift+t`, and
  switch by clicking the `rect` of an entry in `dump.tabs[]`.
* Before `wheel`, point at the terminal body with `mouse_cell`, otherwise the wheel
  may land outside the body.
* `settle` with a budget of at most 2 × `TERMINAL_OUTPUT_BATCH_IDLE` (16 ms) almost
  always returns `settled=false`. Use the default budget unless you need the value.
* `bench`: `gpu_ms` may be empty. `bench … wheel` barely moves the scroll
  animation, because the animation clock is wall time (open question in the ledger).

## Differences from RRiter

* No `open`, `workspace`, `dialog`, `picker_answer`, `record`, `dblclick`: they
  reply `err unknown command '<name>'`.
* No `--profile-from-user`, no `--allow-writes`, no file or folder argument. `shot`
  always opens one terminal tab.
* Default size is `1280x800` (RRiter: `1920x1080`).
