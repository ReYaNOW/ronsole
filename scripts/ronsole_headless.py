#!/usr/bin/env python3
"""Drive `ronsole --headless`: screenshots, command scripts, benchmarks, and a REPL.

No third-party Python modules are required. The script never builds Ronsole;
it runs the release binary (`RONSOLE_BIN` overrides the path) and speaks the
line protocol described in docs/headless.md.

    python3 scripts/ronsole_headless.py shot                  # prints the PNG path
    python3 scripts/ronsole_headless.py bench 480 wheel 0 -3
    python3 scripts/ronsole_headless.py run script.txt --shell /bin/sh
    python3 scripts/ronsole_headless.py run - < script.txt
    python3 scripts/ronsole_headless.py repl
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import subprocess
import sys
from datetime import datetime
from pathlib import Path
from typing import IO, Sequence

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BIN = REPO_ROOT / "target" / "x86_64-unknown-linux-gnu" / "release" / "ronsole"
SHOT_DIR = Path("/tmp/ronsole-headless")
DEFAULT_SETTLE_MS = 500


def classify(line: str) -> str:
    """Protocol reply kind: `ok`, `err`, or `app` for anything the app printed itself."""
    for kind in ("ok", "err"):
        if line == kind or line.startswith(kind + " "):
            return kind
    return "app"


def parse_cli(argv: Sequence[str]) -> argparse.Namespace:
    # Shared flags belong to each subcommand, so they go after it: `run script.txt --shell /bin/sh`.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--size", help="WxH, default 1280x800 (chosen by the binary)")
    common.add_argument("--scale", help="UI scale, default 1.0")
    common.add_argument("--profile", help="reusable profile directory")
    common.add_argument("--keep-profile", action="store_true", help="keep the temp profile on exit")
    common.add_argument("--shell", help="shell started in terminal tabs")
    common.add_argument("--hz", help="frame budget as refresh rate, default 240")
    common.add_argument("--budget-ms", help="frame budget in milliseconds")

    parser = argparse.ArgumentParser(prog="ronsole_headless.py", description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    shot = sub.add_parser("shot", parents=[common], help="open one terminal tab, settle, save a PNG")
    shot.add_argument("--out", help="PNG path, default /tmp/ronsole-headless/ronsole-<ts>-<pid>.png")
    shot.add_argument("--settle", type=int, default=DEFAULT_SETTLE_MS, help="settle budget in ms (default 500)")
    bench = sub.add_parser("bench", parents=[common], help="settle, measure frame cost; prints the summary and CSV path")
    bench.add_argument("frames", type=int, help="number of measured frames")
    bench.add_argument("action", nargs="*", help="per-frame action: wheel DX DY | key COMBO | type TEXT")
    bench.add_argument("--settle", type=int, default=DEFAULT_SETTLE_MS, help="settle budget in ms before measuring (default 500)")
    run = sub.add_parser("run", parents=[common], help="run a command script")
    run.add_argument("script", help="command file, '-' for stdin")
    sub.add_parser("repl", parents=[common], help="type commands interactively")
    return parser.parse_args(argv)


def headless_args(ns: argparse.Namespace) -> list[str]:
    # Conflicts (--hz with --budget-ms, --profile with --keep-profile) are rejected by the binary with exit 2.
    args = ["--headless"]
    for flag, value in (
        ("--size", ns.size),
        ("--scale", ns.scale),
        ("--profile", ns.profile),
        ("--shell", ns.shell),
        ("--hz", ns.hz),
        ("--budget-ms", ns.budget_ms),
    ):
        if value is not None:
            args += [flag, value]
    if ns.keep_profile:
        args.append("--keep-profile")
    return args


def default_shot_path(now: datetime) -> Path:
    # The PID keeps names unique when two runs start in the same second.
    return SHOT_DIR / f"ronsole-{now:%Y%m%d-%H%M%S}-{os.getpid()}.png"


def shot_commands(settle_ms: int, out: Path) -> list[str]:
    return [f"settle {settle_ms}", f"screenshot {out}", "quit"]


def bench_commands(settle_ms: int, frames: int, action: Sequence[str]) -> list[str]:
    return [f"settle {settle_ms}", " ".join(["bench", str(frames), *action]), "quit"]


def find_binary() -> Path | None:
    path = Path(os.environ.get("RONSOLE_BIN") or DEFAULT_BIN)
    if path.is_file():
        return path
    print(f"ronsole binary not found at {path}; run make fast", file=sys.stderr)
    return None


def spawn(binary: Path, args: list[str], stdin: int | None) -> subprocess.Popen[str]:
    # stderr is inherited: app diagnostics and EGL errors reach the caller unchanged.
    return subprocess.Popen(
        [str(binary), *args],
        stdin=stdin,
        stdout=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def read_reply(stdout: IO[str]) -> str | None:
    """Next `ok`/`err` line; other app output goes to stderr. None at EOF."""
    for raw in stdout:
        line = raw.rstrip("\n")
        if classify(line) != "app":
            return line
        print(f"app: {line}", file=sys.stderr)
    return None


def exit_status(code: int) -> int:
    return 128 - code if code < 0 else code


def drive(binary: Path, args: list[str], commands: list[str]) -> tuple[int, list[str]]:
    """Send `commands` one by one; returns (exit code, replies). Any `err` or a missing reply is 1."""
    proc = spawn(binary, args, subprocess.PIPE)
    assert proc.stdin is not None and proc.stdout is not None
    replies: list[str] = []
    failed = False
    for command in commands:
        try:
            proc.stdin.write(command + "\n")
            proc.stdin.flush()
        except BrokenPipeError:
            break
        reply = read_reply(proc.stdout)
        if reply is None:
            break
        replies.append(reply)
        if classify(reply) == "err":
            print(f"{command}: {reply}", file=sys.stderr)
            failed = True
    with contextlib.suppress(BrokenPipeError):
        proc.stdin.close()
    while read_reply(proc.stdout) is not None:
        pass
    code = exit_status(proc.wait())
    if failed:
        return 1, replies
    if code != 0:
        return code, replies
    if len(replies) < len(commands):
        print("ronsole exited before answering every command", file=sys.stderr)
        return 1, replies
    return 0, replies


def cmd_shot(binary: Path, ns: argparse.Namespace) -> int:
    out = Path(ns.out).resolve() if ns.out else default_shot_path(datetime.now())
    code, replies = drive(binary, headless_args(ns), shot_commands(ns.settle, out))
    if code == 0:
        # `screenshot` answers `ok <abs path> <w>x<h>`; the size is the last token.
        print(replies[1].removeprefix("ok ").rsplit(" ", 1)[0])
    return code


def cmd_bench(binary: Path, ns: argparse.Namespace) -> int:
    code, replies = drive(binary, headless_args(ns), bench_commands(ns.settle, ns.frames, ns.action))
    if code == 0:
        summary = replies[1].removeprefix("ok ")
        print(summary)
        print(json.loads(summary).get("csv"))
    return code


def cmd_relay(binary: Path, args: list[str]) -> int:
    """Run with the caller's stdin; protocol replies to stdout, other lines to stderr."""
    proc = spawn(binary, args, None)
    assert proc.stdout is not None
    try:
        while (reply := read_reply(proc.stdout)) is not None:
            print(reply, flush=True)
    except KeyboardInterrupt:
        pass
    return exit_status(proc.wait())


def self_test() -> None:
    for line, kind in [("ok", "ok"), ("ok x", "ok"), ("err x", "err"), ("err", "err"), ("okay", "app"), ("", "app"), ("error x", "app")]:
        assert classify(line) == kind, (line, classify(line))

    ns = parse_cli(["shot", "--size", "800x600", "--scale", "2", "--out", "a.png", "--settle", "250", "--shell", "/bin/sh"])
    assert (ns.command, ns.size, ns.scale, ns.out, ns.settle, ns.shell) == ("shot", "800x600", "2", "a.png", 250, "/bin/sh"), ns
    assert headless_args(ns) == ["--headless", "--size", "800x600", "--scale", "2", "--shell", "/bin/sh"], headless_args(ns)
    ns = parse_cli(["shot"])
    assert (ns.settle, ns.out, ns.size) == (500, None, None), ns
    assert headless_args(ns) == ["--headless"], headless_args(ns)
    ns = parse_cli(["run", "s.txt", "--hz", "144", "--keep-profile"])
    assert (ns.command, ns.script) == ("run", "s.txt"), ns
    assert headless_args(ns) == ["--headless", "--hz", "144", "--keep-profile"], headless_args(ns)
    ns = parse_cli(["repl", "--budget-ms", "4.2"])
    assert ns.command == "repl" and headless_args(ns) == ["--headless", "--budget-ms", "4.2"], ns
    ns = parse_cli(["bench", "480", "wheel", "0", "-3", "--hz", "144"])
    assert (ns.frames, ns.action, ns.settle) == (480, ["wheel", "0", "-3"], 500), ns
    assert headless_args(ns) == ["--headless", "--hz", "144"], headless_args(ns)
    ns = parse_cli(["bench", "5"])
    assert (ns.frames, ns.action) == (5, []), ns
    for bad in ([], ["frob"], ["bench"], ["bench", "many"], ["shot", "x"], ["shot", "--settle", "soon"], ["run"]):
        try:
            with contextlib.redirect_stderr(io.StringIO()):
                parse_cli(bad)
        except SystemExit:
            continue
        raise AssertionError(f"parse_cli accepted {bad}")

    # Expected strings follow the command grammar of docs/headless.md (settle, screenshot, bench).
    now = datetime(2026, 9, 26, 14, 5, 9)
    assert default_shot_path(now) == SHOT_DIR / f"ronsole-20260926-140509-{os.getpid()}.png"
    out = SHOT_DIR / "x.png"
    assert shot_commands(500, out) == ["settle 500", f"screenshot {out}", "quit"]
    assert bench_commands(0, 10, []) == ["settle 0", "bench 10", "quit"]
    assert bench_commands(500, 480, ["wheel", "0", "-3"]) == ["settle 500", "bench 480 wheel 0 -3", "quit"]
    print("ronsole_headless self-test: ok")


def main(argv: Sequence[str]) -> int:
    if list(argv) == ["--self-test"]:
        self_test()
        return 0
    ns = parse_cli(argv)
    binary = find_binary()
    if binary is None:
        return 2
    if ns.command == "shot":
        return cmd_shot(binary, ns)
    if ns.command == "bench":
        return cmd_bench(binary, ns)
    if ns.command == "run":
        args = headless_args(ns)
        # `-` means stdin: the binary reads commands from its own stdin when --script is absent.
        if ns.script != "-":
            args += ["--script", ns.script]
        return cmd_relay(binary, args)
    return cmd_relay(binary, headless_args(ns))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
