#!/usr/bin/env python3
"""Measure Ronsole memory in headless mode: idle, then after a full 120-column scrollback.

Each run starts `ronsole --headless --shell /bin/sh` without WAYLAND_DISPLAY,
samples /proc/<pid>/status and /proc/<pid>/smaps_rollup after an idle period,
fills the scrollback with 10000 lines of 120 columns and samples again.
The table prints the median of the runs in kB.

    python3 scripts/measure_rss.py [BINARY] [--runs N]

The protocol is the one in docs/headless.md; the driver is not imported.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BIN = REPO_ROOT / "target" / "x86_64-unknown-linux-gnu" / "release" / "ronsole"
IDLE_SECONDS = 3.0
TARGET_COLS = 120
FILL_LINES = 10000
MIN_WIDTH = 320
MAX_WIDTH = 8192
FIT_TRIES = 40
FILL_TIMEOUT_S = 300.0
POLL_S = 0.2
# Every line is exactly 120 columns: a zero-padded number, then a newline from echo.
# The script has no backslashes, because the `type` command reads escapes.
FILL_SCRIPT = (
    f"i=0; while [ $i -lt {FILL_LINES} ]; do printf '%0120d' $i; echo; i=$((i+1)); done"
)
LAST_LINE = str(FILL_LINES - 1).rjust(TARGET_COLS, "0")
SMAPS_FIELDS = ("Rss", "Pss", "Private_Dirty")
METRICS = ("VmRSS", *SMAPS_FIELDS)


class RunFailure(Exception):
    """A measurement could not be completed; `status` is the exit code to return."""

    def __init__(self, message: str, status: int = 1) -> None:
        super().__init__(message)
        self.status = status


def classify(line: str) -> str:
    """Protocol reply kind: `ok`, `err`, or `app` for anything the app printed itself."""
    for kind in ("ok", "err"):
        if line == kind or line.startswith(kind + " "):
            return kind
    return "app"


def spawn(binary: Path) -> subprocess.Popen[str]:
    # Only WAYLAND_DISPLAY is removed: the run must not open a window on the user's display.
    env = {key: value for key, value in os.environ.items() if key != "WAYLAND_DISPLAY"}
    # stderr is inherited: app diagnostics and EGL errors reach the caller unchanged.
    return subprocess.Popen(
        [str(binary), "--headless", "--shell", "/bin/sh"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
    )


def read_reply(proc: subprocess.Popen[str]) -> str | None:
    """Next `ok`/`err` line; other app output goes to stderr. None at EOF."""
    assert proc.stdout is not None
    for raw in proc.stdout:
        line = raw.rstrip("\n")
        if classify(line) != "app":
            return line
        print(f"app: {line}", file=sys.stderr)
    return None


def ask(proc: subprocess.Popen[str], command: str) -> str:
    """Send one command and return its `ok` reply; an `err` or a dead process raises RunFailure."""
    assert proc.stdin is not None
    try:
        proc.stdin.write(command + "\n")
        proc.stdin.flush()
    except BrokenPipeError as error:
        raise RunFailure(f"{command!r}: ronsole stdin closed") from error
    reply = read_reply(proc)
    if reply is None:
        code = proc.wait()
        # EGL setup failure is exit 3 (docs/headless.md, Binary CLI); everything else is 1.
        raise RunFailure(
            f"ronsole exited before answering {command!r} (status {code})",
            status=3 if code == 3 else 1,
        )
    if classify(reply) == "err":
        raise RunFailure(f"{command!r}: {reply}")
    return reply


def dump_state(proc: subprocess.Popen[str]) -> dict:
    return json.loads(ask(proc, "dump").removeprefix("ok "))


def sample(pid: int) -> dict[str, int]:
    """VmRSS from /proc/<pid>/status and Rss, Pss, Private_Dirty from smaps_rollup, in kB."""
    values: dict[str, int] = {}
    try:
        for line in Path(f"/proc/{pid}/status").read_text(encoding="utf-8").splitlines():
            key, _, rest = line.partition(":")
            if key == "VmRSS":
                values[key] = int(rest.split()[0])
        for line in Path(f"/proc/{pid}/smaps_rollup").read_text(encoding="utf-8").splitlines():
            key, _, rest = line.partition(":")
            if key in SMAPS_FIELDS:
                values[key] = int(rest.split()[0])
    except OSError as error:
        raise RunFailure(f"cannot read /proc for pid {pid}: {error}") from error
    missing = [metric for metric in METRICS if metric not in values]
    if missing:
        raise RunFailure(f"no {', '.join(missing)} in /proc for pid {pid}")
    return values


def fit_columns(proc: subprocess.Popen[str], width: int, height: int, char_w: float, cols: int) -> None:
    """Resize until the terminal has TARGET_COLS columns: estimate, then bisect on the width."""
    narrow_known: int | None = None  # a width that gives fewer columns than the target
    wide_known: int | None = None  # a width that gives more columns than the target
    for _ in range(FIT_TRIES):
        if cols == TARGET_COLS:
            return
        if cols < TARGET_COLS:
            narrow_known = width if narrow_known is None else max(narrow_known, width)
        else:
            wide_known = width if wide_known is None else min(wide_known, width)
        if narrow_known is None:
            assert wide_known is not None
            next_width = wide_known - max(1, round((cols - TARGET_COLS) * char_w))
        elif wide_known is None:
            next_width = narrow_known + max(1, round((TARGET_COLS - cols) * char_w))
        else:
            if wide_known - narrow_known <= 1:
                raise RunFailure(f"no width gives {TARGET_COLS} columns (char_w {char_w})")
            next_width = (narrow_known + wide_known) // 2
        width = min(MAX_WIDTH, max(MIN_WIDTH, next_width))
        ask(proc, f"resize {width}x{height}")
        ask(proc, "settle")
        cols = dump_state(proc)["terminal"]["cols"]
    raise RunFailure(f"terminal did not reach {TARGET_COLS} columns in {FIT_TRIES} resizes")


def wait_for_last_line(proc: subprocess.Popen[str]) -> None:
    deadline = time.monotonic() + FILL_TIMEOUT_S
    while True:
        rows = dump_state(proc)["terminal"]["text"]
        if any(row.rstrip() == LAST_LINE for row in rows):
            return
        if time.monotonic() > deadline:
            raise RunFailure(f"last line not visible within {FILL_TIMEOUT_S:.0f} s")
        time.sleep(POLL_S)


def run_once(binary: Path) -> tuple[dict[str, int], dict[str, int]]:
    """One fresh process: returns (idle sample, full-scrollback sample)."""
    proc = spawn(binary)
    try:
        state = dump_state(proc)
        layout = state["terminal"].get("layout")
        if not layout or not layout.get("char_w"):
            raise RunFailure("first dump has no terminal.layout.char_w")
        width, height = state["size"]
        time.sleep(IDLE_SECONDS)
        idle = sample(proc.pid)
        fit_columns(proc, width, height, float(layout["char_w"]), state["terminal"]["cols"])
        ask(proc, f"type {FILL_SCRIPT}")
        ask(proc, "key enter")
        wait_for_last_line(proc)
        full = sample(proc.pid)
        ask(proc, "quit")
        assert proc.stdin is not None
        with contextlib.suppress(BrokenPipeError):
            proc.stdin.close()
        while read_reply(proc) is not None:
            pass
        code = proc.wait()
        if code != 0:
            raise RunFailure(f"ronsole exited with status {code} after quit", status=3 if code == 3 else 1)
        return idle, full
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()


def print_table(runs: int, idle: dict[str, list[int]], full: dict[str, list[int]]) -> None:
    print(f"{'median of ' + str(runs) + ' runs, kB':<24}" + "".join(f"{metric:>16}" for metric in METRICS))
    for label, samples in (("idle (3 s)", idle), ("full scrollback", full)):
        medians = [round(statistics.median(samples[metric])) for metric in METRICS]
        print(f"{label:<24}" + "".join(f"{median:>16}" for median in medians))


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", nargs="?", default=str(DEFAULT_BIN), help="ronsole binary (default: release build)")
    parser.add_argument("--runs", type=int, default=3, help="fresh processes to measure, median is printed (default 3)")
    args = parser.parse_args(argv)
    if args.runs < 1:
        parser.error("--runs must be at least 1")
    binary = Path(args.binary)
    if not binary.is_file():
        print(f"ronsole binary not found at {binary}; build the release binary first", file=sys.stderr)
        return 2

    idle: dict[str, list[int]] = {metric: [] for metric in METRICS}
    full: dict[str, list[int]] = {metric: [] for metric in METRICS}
    try:
        for run in range(1, args.runs + 1):
            print(f"run {run}/{args.runs}", file=sys.stderr)
            idle_sample, full_sample = run_once(binary)
            for metric in METRICS:
                idle[metric].append(idle_sample[metric])
                full[metric].append(full_sample[metric])
    except RunFailure as failure:
        print(f"measure_rss: {failure}", file=sys.stderr)
        return failure.status

    non_positive = [
        f"{label} {metric}={value}"
        for label, samples in (("idle", idle), ("full", full))
        for metric, values in samples.items()
        for value in values
        if value <= 0
    ]
    if non_positive:
        print(f"measure_rss: non-positive samples: {', '.join(non_positive)}", file=sys.stderr)
        return 1
    print_table(args.runs, idle, full)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
