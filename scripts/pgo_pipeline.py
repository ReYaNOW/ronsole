#!/usr/bin/env python3
"""Build, train, validate, and reuse Ronsole PGO profiles on Linux/Wayland."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Mapping, Sequence

ROOT = Path(__file__).resolve().parents[1]
SCENARIO_VERSION = 1
DEFAULT_TIMEOUT_SECONDS = 600
PARENT_TIMEOUT_GRACE_SECONDS = 20
TERMINATION_GRACE_SECONDS = 8
PROFILE_PATTERN = "%p_%m.profraw"
EXPECTED_COMPLETED_STEPS = (
    "startup-first-frame",
    "resize-reflow",
    "basic-input-echo",
    "unicode",
    "ansi-style-parser",
    "bulk-output",
    "long-lines-reflow",
    "alternate-screen",
    "scroll",
    "text-selection",
    "search",
    "tabs",
    "settings",
    "process-tree-cleanup",
    "finish",
)
FIXTURE_PHASES = (
    "basic",
    "unicode",
    "ansi",
    "bulk",
    "long-lines",
    "alternate-screen",
    "process-tree",
)


class PgoError(RuntimeError):
    pass


@dataclass(frozen=True)
class PgoPaths:
    root: Path
    target: str
    profile_dir: Path
    generate_target_dir: Path
    use_target_dir: Path
    training_dir: Path
    workspace: Path
    report_path: Path
    merged_profile: Path
    summary_path: Path
    manifest_path: Path
    instrumented_manifest_path: Path
    training_manifest_path: Path


@dataclass(frozen=True)
class PgoConfig:
    root: Path = ROOT
    target: str = ""
    binary_name: str = "ronsole"
    mode: str = "fresh"
    rustflags: str = ""
    build_std_flags: str = ""
    timeout_seconds: int = DEFAULT_TIMEOUT_SECONDS
    cargo_env: Mapping[str, str] = field(default_factory=dict)
    cargo_command: tuple[str, ...] = ("cargo", "+nightly")
    train_only: bool = False
    run_only: bool = False
    run_executable: Path | None = None
    build_only: bool = False
    merge_only: bool = False
    verbose: bool = True

    def validate(self) -> "PgoConfig":
        if self.mode not in {"fresh", "reuse"}:
            raise PgoError(f"unsupported PGO mode: {self.mode}")
        if not self.target:
            raise PgoError("a Rust target triple is required")
        if "linux" not in self.target:
            raise PgoError("Ronsole PGO supports Linux targets only")
        if self.timeout_seconds < 30:
            raise PgoError("PGO automation timeout must be at least 30 seconds")
        special_modes = sum(
            bool(value)
            for value in (self.run_only, self.build_only, self.merge_only)
        )
        if special_modes > 1:
            raise PgoError(
                "--run-only, --build-only, and --merge-only are mutually exclusive"
            )
        if self.train_only and self.mode != "fresh":
            raise PgoError("--train-only requires --mode fresh")
        if self.train_only and special_modes:
            raise PgoError("--train-only cannot be combined with a single-stage mode")
        if self.run_only and self.mode != "fresh":
            raise PgoError("--run-only requires --mode fresh")
        if self.run_executable is not None and not self.run_only:
            raise PgoError("--run-executable requires --run-only")
        if (self.build_only or self.merge_only) and self.mode != "fresh":
            raise PgoError("single-stage build/merge modes require --mode fresh")
        return self


class Runner:
    def __init__(self, *, verbose: bool = True) -> None:
        self.verbose = verbose

    def run(
        self,
        command: Sequence[str | os.PathLike[str]],
        *,
        cwd: Path,
        env: Mapping[str, str] | None = None,
        capture: bool = False,
        check: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        arguments = [os.fspath(part) for part in command]
        if self.verbose:
            print(f"[ronsole-pgo] $ {shlex.join(arguments)}", flush=True)
        return subprocess.run(
            arguments,
            cwd=cwd,
            env=dict(env) if env is not None else None,
            check=check,
            text=True,
            stdout=subprocess.PIPE if capture else None,
            stderr=subprocess.PIPE if capture else None,
        )

    def run_training_process(
        self,
        command: Sequence[str | os.PathLike[str]],
        *,
        cwd: Path,
        env: Mapping[str, str],
        timeout: int,
    ) -> subprocess.CompletedProcess[str]:
        arguments = [os.fspath(part) for part in command]
        if self.verbose:
            print(f"[ronsole-pgo] $ {shlex.join(arguments)}", flush=True)
        process = subprocess.Popen(
            arguments,
            cwd=cwd,
            env=dict(env),
            text=True,
            start_new_session=True,
        )
        try:
            return_code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            self._terminate_process_group(process)
            raise PgoError(
                f"training exceeded parent timeout of {timeout} seconds"
            ) from error
        return subprocess.CompletedProcess(arguments, return_code)

    @staticmethod
    def _terminate_process_group(process: subprocess.Popen[str]) -> None:
        if process.poll() is not None:
            return
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            process.wait(timeout=TERMINATION_GRACE_SECONDS)
            return
        except subprocess.TimeoutExpired:
            pass
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            return
        try:
            process.wait(timeout=TERMINATION_GRACE_SECONDS)
        except subprocess.TimeoutExpired as error:
            raise PgoError("training process group did not terminate") from error


def log_stage(name: str) -> None:
    print(f"[ronsole-pgo] === {name} ===", flush=True)


def _slug(value: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "-_." else "_" for ch in value)


def paths_for(config: PgoConfig) -> PgoPaths:
    root = config.root.resolve()
    target_slug = _slug(config.target)
    profile_dir = root / "target" / "pgo-profiles" / target_slug
    training_dir = root / "target" / "pgo-training" / target_slug
    merged_profile = profile_dir / "merged.profdata"
    return PgoPaths(
        root=root,
        target=config.target,
        profile_dir=profile_dir,
        generate_target_dir=root / "target" / "pgo-generate" / target_slug,
        use_target_dir=root / "target" / "pgo-use" / target_slug,
        training_dir=training_dir,
        workspace=training_dir / "workspace",
        report_path=training_dir / "automation-report.json",
        merged_profile=merged_profile,
        summary_path=profile_dir / "merged.profdata.summary.txt",
        manifest_path=profile_dir / "merged.profdata.manifest.json",
        instrumented_manifest_path=profile_dir / "instrumented-build.manifest.json",
        training_manifest_path=profile_dir / "instrumented-training.manifest.json",
    )


def executable_path(target_dir: Path, target: str, binary_name: str) -> Path:
    return target_dir / target / "release" / binary_name


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _hash_path(digest: "hashlib._Hash", root: Path, path: Path) -> None:
    relative = path.relative_to(root).as_posix().encode("utf-8")
    digest.update(len(relative).to_bytes(4, "big"))
    digest.update(relative)
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)


def source_input_paths(root: Path) -> list[Path]:
    candidates: list[Path] = []
    for name in (
        "Cargo.toml",
        "Cargo.lock",
        "Makefile",
        "build.rs",
        "rust-toolchain",
        "rust-toolchain.toml",
    ):
        path = root / name
        if path.is_file():
            candidates.append(path)
    cargo_config = root / ".cargo" / "config.toml"
    if cargo_config.is_file():
        candidates.append(cargo_config)
    source_root = root / "src"
    if source_root.is_dir():
        candidates.extend(path for path in source_root.rglob("*") if path.is_file())
    pipeline = root / "scripts" / "pgo_pipeline.py"
    if pipeline.is_file():
        candidates.append(pipeline)
    return sorted(set(candidates), key=lambda path: path.relative_to(root).as_posix())


def source_fingerprint(root: Path) -> str:
    digest = hashlib.sha256()
    for path in source_input_paths(root):
        _hash_path(digest, root, path)
    return digest.hexdigest()


def parse_shell_words(value: str, *, option: str) -> list[str]:
    if not value.strip():
        return []
    try:
        return shlex.split(value)
    except ValueError as error:
        raise PgoError(f"invalid {option}: {error}") from error


def base_environment(config: PgoConfig) -> dict[str, str]:
    environment = dict(os.environ)
    environment.update({str(key): str(value) for key, value in config.cargo_env.items()})
    return environment


def build_environment(
    config: PgoConfig,
    *,
    target_dir: Path,
    pgo_flags: Sequence[str],
    panic_strategy: str,
) -> dict[str, str]:
    environment = base_environment(config)
    environment["CARGO_TARGET_DIR"] = str(target_dir)
    environment["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"] = "1"
    environment["CARGO_PROFILE_RELEASE_PANIC"] = panic_strategy
    flags = parse_shell_words(config.rustflags, option="--rustflags")
    flags.extend(pgo_flags)
    environment.pop("RUSTFLAGS", None)
    if flags:
        environment["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    else:
        environment.pop("CARGO_ENCODED_RUSTFLAGS", None)
    return environment


def cargo_build_command(config: PgoConfig) -> list[str]:
    command = [*config.cargo_command, "build", "--locked", "--offline"]
    command.extend(
        parse_shell_words(config.build_std_flags, option="--build-std-flags")
    )
    command.extend(
        ["--target", config.target, "--release", "--bin", config.binary_name]
    )
    return command


def rustc_identity(config: PgoConfig, runner: Runner) -> str:
    result = runner.run(
        ["rustup", "run", "nightly", "rustc", "-Vv"],
        cwd=config.root,
        env=base_environment(config),
        capture=True,
    )
    identity = result.stdout.strip()
    if not identity:
        raise PgoError("nightly rustc returned an empty identity")
    return identity


def llvm_profdata_command(*arguments: str | os.PathLike[str]) -> list[str | os.PathLike[str]]:
    return ["rustup", "run", "nightly", "llvm-profdata", *arguments]


def llvm_profdata_identity(config: PgoConfig, runner: Runner) -> str:
    result = runner.run(
        llvm_profdata_command("--version"),
        cwd=config.root,
        env=base_environment(config),
        capture=True,
    )
    identity = result.stdout.strip()
    if not identity:
        raise PgoError("llvm-profdata returned an empty identity")
    return identity


def validate_project_paths(config: PgoConfig) -> None:
    required = (
        config.root / "Cargo.toml",
        config.root / "Cargo.lock",
        config.root / "Makefile",
        config.root / "src" / "main.rs",
    )
    missing = [str(path) for path in required if not path.is_file()]
    if missing:
        raise PgoError("required project paths are missing: " + ", ".join(missing))


def validate_immediate_abort_cargo_opt_in(config: PgoConfig) -> None:
    manifest_path = config.root / "Cargo.toml"
    try:
        with manifest_path.open("rb") as source:
            manifest = tomllib.load(source)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise PgoError(
            f"failed to parse Cargo.toml for final panic contract: {error}"
        ) from error

    cargo_features = manifest.get("cargo-features", [])
    if not isinstance(cargo_features, list) or not all(
        isinstance(feature, str) for feature in cargo_features
    ):
        raise PgoError("Cargo.toml cargo-features must be an array of strings")
    if "panic-immediate-abort" not in cargo_features:
        raise PgoError(
            "final PGO panic=immediate-abort requires Cargo.toml opt-in: "
            'cargo-features = ["panic-immediate-abort"]'
        )


def validate_linux_host() -> None:
    if not sys.platform.startswith("linux"):
        raise PgoError("Ronsole PGO automation is Linux-only")


def validate_toolchain(config: PgoConfig, runner: Runner) -> None:
    if shutil.which("rustup") is None:
        raise PgoError("rustup is required for the nightly PGO toolchain")
    if shutil.which("cargo") is None:
        raise PgoError("cargo is required for PGO builds")
    runner.run(
        [*config.cargo_command, "--version"],
        cwd=config.root,
        env=base_environment(config),
        capture=True,
    )
    rustc_identity(config, runner)
    llvm_profdata_identity(config, runner)


def resolve_wayland_socket(environment: Mapping[str, str]) -> Path:
    display = environment.get("WAYLAND_DISPLAY", "")
    if not display:
        raise PgoError("WAYLAND_DISPLAY is required for Ronsole PGO training")
    display_path = Path(display)
    if display_path.is_absolute():
        socket_path = display_path
    else:
        runtime = environment.get("XDG_RUNTIME_DIR", "")
        if not runtime:
            raise PgoError(
                "XDG_RUNTIME_DIR is required when WAYLAND_DISPLAY is relative"
            )
        socket_path = Path(runtime) / display_path
    try:
        mode = socket_path.stat().st_mode
    except OSError as error:
        raise PgoError(f"Wayland socket is not accessible: {socket_path}: {error}") from error
    if not stat.S_ISSOCK(mode):
        raise PgoError(f"WAYLAND_DISPLAY does not point to a socket: {socket_path}")
    return socket_path.absolute()


def _ensure_private_directory(path: Path) -> None:
    path.mkdir(parents=True, mode=0o700, exist_ok=True)
    path.chmod(0o700)


def private_training_runtime() -> tempfile.TemporaryDirectory:
    return tempfile.TemporaryDirectory(
        prefix=f"ronsole-pgo-{os.geteuid()}-",
        dir="/tmp",
        ignore_cleanup_errors=True,
    )


def isolated_training_environment(
    config: PgoConfig,
    paths: PgoPaths,
    *,
    wayland_socket: Path,
    profile_dir: Path,
    private_runtime: Path,
) -> dict[str, str]:
    environment = base_environment(config)
    home = paths.workspace / "home"
    config_home = paths.workspace / "config"
    cache_home = paths.workspace / "cache"
    data_home = paths.workspace / "data"
    state_home = paths.workspace / "state"
    for directory in (home, config_home, cache_home, data_home, state_home):
        directory.mkdir(parents=True, exist_ok=True)
    _ensure_private_directory(private_runtime)
    profile_dir.mkdir(parents=True, exist_ok=True)
    environment.update(
        {
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(config_home),
            "XDG_CACHE_HOME": str(cache_home),
            "XDG_DATA_HOME": str(data_home),
            "XDG_STATE_HOME": str(state_home),
            "XDG_RUNTIME_DIR": str(private_runtime),
            "WAYLAND_DISPLAY": str(wayland_socket),
            "SHELL": "/bin/sh",
            "LLVM_PROFILE_FILE": str(profile_dir / PROFILE_PATTERN),
            "RUST_BACKTRACE": "1",
        }
    )
    return environment


def terminal_fixture_text() -> str:
    return """#!/bin/sh
set -eu
phase=${1:-}
case "$phase" in
  basic)
    printf 'ronsole-pgo basic\\nalpha beta gamma\\n0123456789\\n'
    ;;
  unicode)
    printf 'Latin: cafe\\nCyrillic: Привет мир\\nCJK: 界中語\\nCombining: é å\\nEmoji: 😀 🚀 ✅\\n'
    ;;
  ansi)
    printf '\\033[0mnormal reset\\n'
    i=30
    while [ "$i" -le 37 ]; do printf '\\033[%smcolor-%s\\033[0m ' "$i" "$i"; i=$((i + 1)); done
    printf '\\n'
    i=90
    while [ "$i" -le 97 ]; do printf '\\033[%smbright-%s\\033[0m ' "$i" "$i"; i=$((i + 1)); done
    printf '\\n\\033[38;5;196m256-color\\033[0m \\033[38;2;12;180;220mtruecolor\\033[0m\\n'
    printf '\\033[1mbold\\033[0m \\033[2mdim\\033[0m \\033[3mitalic\\033[0m \\033[4munderline\\033[0m \\033[9mstrike\\033[0m \\033[7minverse\\033[0m\\n'
    printf 'cursor-A\\033[2DXY\\nline-to-erase\\033[2K\\rprogress-1\\rprogress-2\\n'
    printf '\\033[2J\\033[Hafter-erase-display\\n'
    ;;
  bulk)
    i=0
    while [ "$i" -lt 4096 ]; do
      case $((i % 4)) in
        0) printf 'bulk %04d short\\n' "$i" ;;
        1) printf 'bulk %04d medium abcdefghijklmnopqrstuvwxyz 0123456789\\n' "$i" ;;
        2) printf '\\033[32mbulk %04d colored output\\033[0m\\n' "$i" ;;
        3) printf 'bulk %04d mixed Привет 界 😀\\n' "$i" ;;
      esac
      i=$((i + 1))
    done
    ;;
  long-lines)
    chunk='0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ'
    i=0
    while [ "$i" -lt 24 ]; do
      printf 'long-ascii-%02d ' "$i"
      j=0
      while [ "$j" -lt 16 ]; do printf '%s' "$chunk"; j=$((j + 1)); done
      printf '\\nlong-unicode-%02d ' "$i"
      j=0
      while [ "$j" -lt 32 ]; do printf 'Привет-界-😀-'; j=$((j + 1)); done
      printf '\\n'
      i=$((i + 1))
    done
    ;;
  alternate-screen)
    printf '\\033[?1049h\\033[2J\\033[Halt-screen frame 1\\n'
    sleep 0.25
    printf '\\033[2;1Halt-screen frame 2\\033[3;1Hcursor movement\\n'
    sleep 0.25
    printf '\\033[1;1Halt-screen frame 3 updated\\n'
    sleep 0.25
    printf '\\033[?1049lprimary-screen-restored\\n'
    ;;
  process-tree)
    /bin/sh -c '(sleep 3) & child=$!; wait "$child"' &
    parent=$!
    wait "$parent"
    printf 'process-tree complete\\n'
    ;;
  *)
    printf 'usage: %s {%s}\\n' "$0" 'basic|unicode|ansi|bulk|long-lines|alternate-screen|process-tree' >&2
    exit 2
    ;;
esac
"""


def create_training_workspace(paths: PgoPaths) -> None:
    if paths.training_dir.exists():
        shutil.rmtree(paths.training_dir)
    paths.workspace.mkdir(parents=True, exist_ok=True)
    fixture = paths.workspace / "terminal_fixture.sh"
    fixture.write_text(terminal_fixture_text(), encoding="utf-8")
    fixture.chmod(0o755)


def training_command(config: PgoConfig, paths: PgoPaths, executable: Path) -> list[str]:
    return [
        str(executable),
        "--pgo-train",
        "--pgo-workspace",
        str(paths.workspace.resolve()),
        "--pgo-report",
        str(paths.report_path.resolve()),
        "--pgo-timeout-seconds",
        str(config.timeout_seconds),
    ]


def _validate_string_list(value: object, *, field_name: str, allow_empty: bool) -> list[str]:
    if not isinstance(value, list) or not all(
        isinstance(item, str) and item for item in value
    ):
        raise PgoError(f"automation report field {field_name!r} must be a string list")
    if not allow_empty and not value:
        raise PgoError(f"automation report field {field_name!r} must not be empty")
    return value


def validate_training_report(report: Mapping[str, object]) -> None:
    status_value = report.get("status")
    if status_value != "ok":
        failed_step = report.get("failed_step")
        error = report.get("error")
        detail = f"failed_step={failed_step!r} error={error!r}"
        raise PgoError(f"Ronsole automation did not succeed: {detail}")
    scenario_version = report.get("scenario_version")
    if isinstance(scenario_version, bool) or scenario_version != SCENARIO_VERSION:
        raise PgoError(
            "automation report scenario version mismatch: "
            f"expected {SCENARIO_VERSION}, got {scenario_version!r}"
        )
    completed_steps = _validate_string_list(
        report.get("completed_steps"), field_name="completed_steps", allow_empty=False
    )
    if tuple(completed_steps) != EXPECTED_COMPLETED_STEPS:
        raise PgoError(
            "automation report completed_steps does not exactly match "
            f"scenario version {SCENARIO_VERSION}"
        )
    _validate_string_list(
        report.get("skipped_steps"), field_name="skipped_steps", allow_empty=True
    )
    duration_ms = report.get("duration_ms")
    if (
        isinstance(duration_ms, bool)
        or not isinstance(duration_ms, int)
        or duration_ms < 0
    ):
        raise PgoError("automation report duration_ms must be a non-negative integer")


def load_training_report(path: Path) -> dict[str, object]:
    if not path.is_file():
        raise PgoError(f"automation report was not created: {path}")
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise PgoError(f"invalid automation report {path}: {error}") from error
    if not isinstance(payload, dict):
        raise PgoError("automation report root must be a JSON object")
    validate_training_report(payload)
    return payload


def raw_profiles(profile_dir: Path) -> list[Path]:
    return sorted(
        path
        for path in profile_dir.glob("*.profraw")
        if path.is_file() and path.stat().st_size > 0
    )


def _profile_snapshot(profile_dir: Path) -> dict[Path, tuple[int, int]]:
    snapshot: dict[Path, tuple[int, int]] = {}
    for path in profile_dir.glob("*.profraw"):
        if not path.is_file():
            continue
        current = path.stat()
        snapshot[path] = (current.st_size, current.st_mtime_ns)
    return snapshot


def _new_or_changed_profiles(
    before: Mapping[Path, tuple[int, int]], profile_dir: Path
) -> list[Path]:
    created: list[Path] = []
    for path in raw_profiles(profile_dir):
        current = path.stat()
        state = (current.st_size, current.st_mtime_ns)
        if before.get(path) != state:
            created.append(path)
    return created


def run_training(
    config: PgoConfig,
    paths: PgoPaths,
    executable: Path,
    runner: Runner,
    *,
    profile_dir: Path,
    require_profile: bool,
) -> tuple[dict[str, object], list[Path]]:
    validate_linux_host()
    wayland_socket = resolve_wayland_socket(base_environment(config))
    profile_dir.mkdir(parents=True, exist_ok=True)
    before_profiles = _profile_snapshot(profile_dir)
    command = training_command(config, paths, executable)
    with private_training_runtime() as private_runtime_name:
        environment = isolated_training_environment(
            config,
            paths,
            wayland_socket=wayland_socket,
            profile_dir=profile_dir,
            private_runtime=Path(private_runtime_name),
        )
        result = runner.run_training_process(
            command,
            cwd=paths.workspace,
            env=environment,
            timeout=config.timeout_seconds + PARENT_TIMEOUT_GRACE_SECONDS,
        )
        report_error: PgoError | None = None
        try:
            report = load_training_report(paths.report_path)
        except PgoError as error:
            report_error = error
            report = {}
        if result.returncode != 0:
            suffix = f"; {report_error}" if report_error is not None else ""
            raise PgoError(
                f"Ronsole PGO training exited with code {result.returncode}{suffix}"
            )
        if report_error is not None:
            raise report_error
        generated = _new_or_changed_profiles(before_profiles, profile_dir)
        if require_profile:
            if not generated:
                raise PgoError(
                    "training completed without creating a new non-empty .profraw profile"
                )
        return report, generated


def build_instrumented(config: PgoConfig, paths: PgoPaths, runner: Runner) -> Path:
    log_stage("instrumented MAX-like build")
    paths.profile_dir.mkdir(parents=True, exist_ok=True)
    environment = build_environment(
        config,
        target_dir=paths.generate_target_dir,
        pgo_flags=[f"-Cprofile-generate={paths.profile_dir}"],
        panic_strategy="abort",
    )
    runner.run(
        cargo_build_command(config),
        cwd=paths.root,
        env=environment,
    )
    executable = executable_path(
        paths.generate_target_dir, config.target, config.binary_name
    )
    if not executable.is_file():
        raise PgoError(f"instrumented Ronsole executable not found: {executable}")
    write_instrumented_build_manifest(config, paths, runner, executable)
    return executable


def build_with_profile(config: PgoConfig, paths: PgoPaths, runner: Runner) -> Path:
    log_stage("validate reusable profile")
    validate_profile(config, paths, runner)
    log_stage("final MAX-equivalent profile-use build")
    if paths.use_target_dir.exists():
        shutil.rmtree(paths.use_target_dir)
    environment = build_environment(
        config,
        target_dir=paths.use_target_dir,
        pgo_flags=[
            f"-Cprofile-use={paths.merged_profile}",
            "-Cllvm-args=-pgo-warn-missing-function",
        ],
        panic_strategy="immediate-abort",
    )
    runner.run(
        cargo_build_command(config),
        cwd=paths.root,
        env=environment,
    )
    executable = executable_path(paths.use_target_dir, config.target, config.binary_name)
    if not executable.is_file():
        raise PgoError(f"PGO Ronsole executable not found: {executable}")
    return executable


def merge_profiles(
    config: PgoConfig,
    paths: PgoPaths,
    runner: Runner,
    profiles: Sequence[Path],
) -> None:
    log_stage("verify and merge .profraw")
    if not profiles:
        raise PgoError(f"no non-empty .profraw files found in {paths.profile_dir}")
    paths.profile_dir.mkdir(parents=True, exist_ok=True)
    runner.run(
        llvm_profdata_command(
            "merge", "-sparse", "-o", paths.merged_profile, *profiles
        ),
        cwd=paths.root,
        env=base_environment(config),
    )
    if not paths.merged_profile.is_file() or paths.merged_profile.stat().st_size == 0:
        raise PgoError(f"llvm-profdata did not create {paths.merged_profile}")
    summary = runner.run(
        llvm_profdata_command("show", "--counts", paths.merged_profile),
        cwd=paths.root,
        env=base_environment(config),
        capture=True,
    )
    if not summary.stdout.strip():
        raise PgoError("llvm-profdata produced an empty profile summary")
    paths.summary_path.write_text(summary.stdout, encoding="utf-8")


def compatibility_payload(
    config: PgoConfig,
    runner: Runner,
    *,
    release_panic: str = "immediate-abort",
) -> dict[str, object]:
    cargo_toml = config.root / "Cargo.toml"
    cargo_lock = config.root / "Cargo.lock"
    if not cargo_toml.is_file() or not cargo_lock.is_file():
        raise PgoError("Cargo.toml and Cargo.lock are required for profile compatibility")
    cargo_profile_environment = {
        key: str(value)
        for key, value in sorted(base_environment(config).items())
        if key.startswith("CARGO_PROFILE_")
        and key not in {
            "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
            "CARGO_PROFILE_RELEASE_PANIC",
        }
    }
    cargo_profile_environment.update(
        {
            "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
            "CARGO_PROFILE_RELEASE_PANIC": release_panic,
        }
    )
    return {
        "schema": 1,
        "scenario_version": SCENARIO_VERSION,
        "target": config.target,
        "binary_name": config.binary_name,
        "rustc": rustc_identity(config, runner),
        "llvm_profdata": llvm_profdata_identity(config, runner),
        "max_rustflags": parse_shell_words(config.rustflags, option="--rustflags"),
        "build_std_flags": parse_shell_words(
            config.build_std_flags, option="--build-std-flags"
        ),
        "instrumented_panic": "abort",
        "final_panic": "immediate-abort",
        "cargo_profile_environment": cargo_profile_environment,
        "cargo_toml_sha256": sha256_file(cargo_toml),
        "cargo_lock_sha256": sha256_file(cargo_lock),
        "source_fingerprint_sha256": source_fingerprint(config.root),
    }


def write_json_atomic(path: Path, payload: Mapping[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    temporary.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    os.replace(temporary, path)


def instrumented_build_payload(
    config: PgoConfig,
    runner: Runner,
    executable: Path,
) -> dict[str, object]:
    resolved_root = config.root.resolve()
    resolved_executable = executable.resolve()
    try:
        executable_identity = resolved_executable.relative_to(resolved_root).as_posix()
    except ValueError as error:
        raise PgoError(
            f"instrumented executable is outside the project root: {resolved_executable}"
        ) from error
    payload = compatibility_payload(config, runner, release_panic="abort")
    payload.update(
        {
            "manifest_kind": "instrumented-build",
            "instrumented_executable": executable_identity,
            "instrumented_executable_sha256": sha256_file(resolved_executable),
        }
    )
    return payload


def write_instrumented_build_manifest(
    config: PgoConfig,
    paths: PgoPaths,
    runner: Runner,
    executable: Path,
) -> None:
    payload = instrumented_build_payload(config, runner, executable)
    payload["created_unix_seconds"] = int(time.time())
    write_json_atomic(paths.instrumented_manifest_path, payload)


def write_manifest(
    config: PgoConfig,
    paths: PgoPaths,
    runner: Runner,
    profiles: Sequence[Path],
    report: Mapping[str, object],
) -> None:
    validate_instrumented_build(config, paths, runner)
    validate_training_report(report)
    payload = compatibility_payload(config, runner)
    payload.update(
        {
            "manifest_kind": "merged-profile",
            "instrumented_build_manifest_sha256": sha256_file(
                paths.instrumented_manifest_path
            ),
            "merged_profile_sha256": sha256_file(paths.merged_profile),
            "profile_summary_sha256": sha256_file(paths.summary_path),
            "raw_profile_count": len(profiles),
            "completed_steps": report.get("completed_steps", []),
            "skipped_steps": report.get("skipped_steps", []),
            "automation_report_sha256": sha256_file(paths.report_path),
            "created_unix_seconds": int(time.time()),
        }
    )
    write_json_atomic(paths.manifest_path, payload)


def _load_manifest(path: Path) -> dict[str, object]:
    if not path.is_file():
        raise PgoError(
            f"PGO manifest not found: {path}; create a fresh profile before reuse"
        )
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise PgoError(f"invalid PGO manifest {path}: {error}") from error
    if not isinstance(payload, dict):
        raise PgoError("PGO manifest root must be a JSON object")
    return payload


def _instrumented_stale(reason: str) -> PgoError:
    return PgoError(f"instrumented build is stale ({reason}); run `make pgo-gen`")


def validate_instrumented_build(
    config: PgoConfig,
    paths: PgoPaths,
    runner: Runner,
) -> Path:
    executable = executable_path(
        paths.generate_target_dir, config.target, config.binary_name
    ).resolve()
    if not paths.instrumented_manifest_path.is_file():
        raise _instrumented_stale("generation manifest is missing")
    if not executable.is_file():
        raise _instrumented_stale(f"instrumented executable is missing: {executable}")
    try:
        manifest = _load_manifest(paths.instrumented_manifest_path)
        expected = instrumented_build_payload(config, runner, executable)
    except PgoError as error:
        raise _instrumented_stale(str(error)) from error
    mismatches = [
        key
        for key, expected_value in expected.items()
        if manifest.get(key) != expected_value
    ]
    if mismatches:
        raise _instrumented_stale(
            "generation provenance mismatch: " + ", ".join(sorted(mismatches))
        )
    return executable


def _profile_records(paths: PgoPaths, profiles: Sequence[Path]) -> list[dict[str, str]]:
    records: list[dict[str, str]] = []
    for profile in sorted(profiles):
        resolved = profile.resolve()
        try:
            identity = resolved.relative_to(paths.profile_dir.resolve()).as_posix()
        except ValueError as error:
            raise PgoError(
                f"raw PGO profile is outside the profile directory: {resolved}"
            ) from error
        records.append({"path": identity, "sha256": sha256_file(resolved)})
    return records


def write_training_manifest(
    paths: PgoPaths,
    report: Mapping[str, object],
    generated_profiles: Sequence[Path],
) -> None:
    validate_training_report(report)
    current_profiles = raw_profiles(paths.profile_dir)
    if sorted(generated_profiles) != current_profiles or not current_profiles:
        raise PgoError(
            "instrumented training profiles are incomplete or include stale data; "
            "run `make pgo-run` again"
        )
    if not paths.instrumented_manifest_path.is_file():
        raise _instrumented_stale("generation manifest is missing")
    if not paths.report_path.is_file():
        raise PgoError("automation report is missing after successful training")
    payload: dict[str, object] = {
        "schema": 1,
        "manifest_kind": "instrumented-training",
        "scenario_version": SCENARIO_VERSION,
        "instrumented_build_manifest_sha256": sha256_file(
            paths.instrumented_manifest_path
        ),
        "automation_report_sha256": sha256_file(paths.report_path),
        "profiles": _profile_records(paths, current_profiles),
        "created_unix_seconds": int(time.time()),
    }
    write_json_atomic(paths.training_manifest_path, payload)


def validate_training_provenance(paths: PgoPaths) -> list[Path]:
    if not paths.training_manifest_path.is_file():
        raise PgoError(
            "instrumented training provenance is missing; run `make pgo-run` first"
        )
    manifest = _load_manifest(paths.training_manifest_path)
    if manifest.get("schema") != 1:
        raise PgoError("instrumented training provenance has the wrong schema")
    if manifest.get("manifest_kind") != "instrumented-training":
        raise PgoError("instrumented training provenance has the wrong manifest kind")
    if manifest.get("scenario_version") != SCENARIO_VERSION:
        raise PgoError("instrumented training provenance has the wrong scenario version")
    if not paths.instrumented_manifest_path.is_file():
        raise _instrumented_stale("generation manifest is missing")
    if manifest.get("instrumented_build_manifest_sha256") != sha256_file(
        paths.instrumented_manifest_path
    ):
        raise PgoError(
            "instrumented training provenance does not match the current generation; "
            "run `make pgo-run` again"
        )
    if not paths.report_path.is_file() or manifest.get(
        "automation_report_sha256"
    ) != sha256_file(paths.report_path):
        raise PgoError(
            "instrumented training provenance does not match the automation report; "
            "run `make pgo-run` again"
        )
    profiles = raw_profiles(paths.profile_dir)
    if not profiles or manifest.get("profiles") != _profile_records(paths, profiles):
        raise PgoError(
            "instrumented training provenance does not match raw profiles; "
            "run `make pgo-run` again"
        )
    return profiles


def validate_training_for_merge(
    config: PgoConfig,
    paths: PgoPaths,
    runner: Runner,
) -> tuple[dict[str, object], list[Path]]:
    validate_instrumented_build(config, paths, runner)
    report = load_training_report(paths.report_path)
    profiles = validate_training_provenance(paths)
    return report, profiles


def validate_profile(config: PgoConfig, paths: PgoPaths, runner: Runner) -> None:
    if not paths.merged_profile.is_file() or paths.merged_profile.stat().st_size == 0:
        raise PgoError(
            f"PGO profile not found: {paths.merged_profile}; create a fresh profile first"
        )
    if not paths.summary_path.is_file() or paths.summary_path.stat().st_size == 0:
        raise PgoError(f"PGO profile summary not found: {paths.summary_path}")
    manifest = _load_manifest(paths.manifest_path)
    expected = compatibility_payload(config, runner)
    mismatches = [
        key for key, expected_value in expected.items() if manifest.get(key) != expected_value
    ]
    if manifest.get("merged_profile_sha256") != sha256_file(paths.merged_profile):
        mismatches.append("merged_profile_sha256")
    if manifest.get("profile_summary_sha256") != sha256_file(paths.summary_path):
        mismatches.append("profile_summary_sha256")
    if mismatches:
        raise PgoError(
            "saved PGO profile is stale or incompatible ("
            + ", ".join(sorted(set(mismatches)))
            + "); create a fresh profile"
        )
    summary = runner.run(
        llvm_profdata_command("show", "--counts", paths.merged_profile),
        cwd=paths.root,
        env=base_environment(config),
        capture=True,
    )
    if not summary.stdout.strip():
        raise PgoError("saved PGO profile has an empty llvm-profdata summary")


def existing_run_executable(config: PgoConfig, paths: PgoPaths) -> Path:
    if config.run_executable is None:
        raise PgoError("internal error: explicit run executable is required")
    executable = config.run_executable
    if not executable.is_absolute():
        executable = paths.root / executable
    executable = executable.resolve()
    if not executable.is_file():
        raise PgoError(
            f"fast automation Ronsole executable is missing: {executable}; "
            "run `make pgo-gen-fast` first"
        )
    return executable


def _prepare_fresh_profile_dir(paths: PgoPaths) -> None:
    if paths.profile_dir.exists():
        shutil.rmtree(paths.profile_dir)
    paths.profile_dir.mkdir(parents=True, exist_ok=True)


def _prepare_instrumented_training_artifacts(paths: PgoPaths) -> None:
    for profile in paths.profile_dir.glob("*.profraw"):
        if profile.is_file():
            profile.unlink()
    for path in (
        paths.merged_profile,
        paths.summary_path,
        paths.manifest_path,
        paths.training_manifest_path,
    ):
        if path.is_file():
            path.unlink()


def run_pipeline(config: PgoConfig, *, runner: Runner | None = None) -> Path | None:
    config = config.validate()
    runner = Runner(verbose=config.verbose) if runner is None else runner
    validate_linux_host()
    validate_project_paths(config)
    if not (config.run_only and config.run_executable is not None):
        validate_immediate_abort_cargo_opt_in(config)
    paths = paths_for(config)

    if config.run_only:
        if config.run_executable is None:
            log_stage("validate instrumented build provenance")
            validate_toolchain(config, runner)
            executable = validate_instrumented_build(config, paths, runner)
        else:
            executable = existing_run_executable(config, paths)
        log_stage("preflight Wayland training environment")
        resolve_wayland_socket(base_environment(config))
        if config.run_executable is None:
            _prepare_instrumented_training_artifacts(paths)
        create_training_workspace(paths)
        if config.run_executable is None:
            profile_dir = paths.profile_dir
            require_profile = True
        else:
            profile_dir = paths.training_dir / "script-profiles"
            require_profile = False
        log_stage("automated real Wayland Ronsole training")
        report, generated_profiles = run_training(
            config,
            paths,
            executable,
            runner,
            profile_dir=profile_dir,
            require_profile=require_profile,
        )
        if config.run_executable is None:
            write_training_manifest(paths, report, generated_profiles)
        print(f"[ronsole-pgo] automation report: {paths.report_path}", flush=True)
        return None

    log_stage("preflight project and nightly PGO tools")
    validate_toolchain(config, runner)

    if config.build_only:
        _prepare_fresh_profile_dir(paths)
        build_instrumented(config, paths, runner)
        return None

    if config.merge_only:
        report, profiles = validate_training_for_merge(config, paths, runner)
        merge_profiles(config, paths, runner, profiles)
        write_manifest(config, paths, runner, profiles, report)
        print(f"[ronsole-pgo] profile: {paths.merged_profile}", flush=True)
        return None

    if config.mode == "fresh":
        log_stage("preflight Wayland training environment")
        resolve_wayland_socket(base_environment(config))
        _prepare_fresh_profile_dir(paths)
        create_training_workspace(paths)
        build_instrumented(config, paths, runner)
        executable = validate_instrumented_build(config, paths, runner)
        log_stage("automated real Wayland Ronsole training")
        report, generated_profiles = run_training(
            config,
            paths,
            executable,
            runner,
            profile_dir=paths.profile_dir,
            require_profile=True,
        )
        write_training_manifest(paths, report, generated_profiles)
        report, profiles = validate_training_for_merge(config, paths, runner)
        merge_profiles(config, paths, runner, profiles)
        write_manifest(config, paths, runner, profiles, report)
        print(f"[ronsole-pgo] profile: {paths.merged_profile}", flush=True)
        if config.train_only:
            return None

    return build_with_profile(config, paths, runner)


def parse_env(values: Sequence[str]) -> dict[str, str]:
    environment: dict[str, str] = {}
    for value in values:
        key, separator, item = value.partition("=")
        if not separator or not key:
            raise PgoError(f"invalid --env value {value!r}; expected NAME=VALUE")
        environment[key] = item
    return environment


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target")
    parser.add_argument("--binary-name", default="ronsole")
    parser.add_argument("--mode", choices=("fresh", "reuse"), default="fresh")
    parser.add_argument("--rustflags", default="")
    parser.add_argument("--build-std-flags", default="")
    parser.add_argument("--timeout-seconds", type=int, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--env", action="append", default=[], metavar="NAME=VALUE")
    parser.add_argument("--train-only", action="store_true")
    parser.add_argument("--run-only", action="store_true")
    parser.add_argument("--run-executable", type=Path)
    parser.add_argument("--build-only", action="store_true")
    parser.add_argument("--merge-only", action="store_true")
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args(argv)


class _SelfTestRunner(Runner):
    def __init__(
        self,
        *,
        rustc: str = "rustc 1.99.0-nightly (self-test)\ncommit-hash: self-test",
        llvm_profdata: str = "LLVM version self-test",
    ) -> None:
        super().__init__(verbose=False)
        self.rustc = rustc
        self.llvm_profdata = llvm_profdata

    def run(
        self,
        command: Sequence[str | os.PathLike[str]],
        *,
        cwd: Path,
        env: Mapping[str, str] | None = None,
        capture: bool = False,
        check: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        del cwd, env, capture, check
        arguments = [os.fspath(part) for part in command]
        joined = " ".join(arguments)
        if "rustc" in joined:
            output = self.rustc
        elif "llvm-profdata" in joined and "--version" in arguments:
            output = self.llvm_profdata
        elif "llvm-profdata" in joined and "show" in arguments:
            output = "Instrumentation level: Front-end\nFunctions shown: 1\n"
        else:
            output = "self-test\n"
        return subprocess.CompletedProcess(arguments, 0, stdout=output, stderr="")


def _expect_pgo_error(action: object, message: str) -> None:
    try:
        callable_action = action
        if not callable(callable_action):
            raise AssertionError("self-test action is not callable")
        callable_action()
    except PgoError:
        return
    raise PgoError(message)


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="ronsole-pgo-selftest-") as directory:
        root = Path(directory)
        (root / "src").mkdir()
        (root / "scripts").mkdir()
        (root / ".cargo").mkdir()
        cargo_manifest = root / "Cargo.toml"
        valid_cargo_manifest = (
            'cargo-features = ["panic-immediate-abort"]\n\n'
            "[package]\nname='ronsole'\nversion='0.0.0'\n"
        )
        cargo_manifest.write_text(valid_cargo_manifest, encoding="utf-8")
        (root / "Cargo.lock").write_text("# self-test\n", encoding="utf-8")
        (root / "Makefile").write_text("MAX_RUSTFLAGS := -C opt-level=3\n", encoding="utf-8")
        (root / ".cargo" / "config.toml").write_text("[net]\noffline=true\n", encoding="utf-8")
        source = root / "src" / "main.rs"
        source.write_text("fn main() {}\n", encoding="utf-8")
        pipeline = root / "scripts" / "pgo_pipeline.py"
        pipeline.write_text("# self-test pipeline\n", encoding="utf-8")

        config = PgoConfig(
            root=root,
            target="x86_64-unknown-linux-gnu",
            rustflags="-C target-cpu=native -C lto=fat",
            build_std_flags="-Z build-std=core,alloc,std,panic_abort,test",
            cargo_env={
                "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
                "CARGO_PROFILE_RELEASE_PANIC": "immediate-abort",
            },
        )
        paths = paths_for(config)
        if paths.profile_dir != root / "target" / "pgo-profiles" / config.target:
            raise PgoError("profile path layout self-test failed")
        if paths.generate_target_dir != root / "target" / "pgo-generate" / config.target:
            raise PgoError("generate target path layout self-test failed")
        if paths.use_target_dir != root / "target" / "pgo-use" / config.target:
            raise PgoError("profile-use target path layout self-test failed")
        if paths.training_dir != root / "target" / "pgo-training" / config.target:
            raise PgoError("training path layout self-test failed")

        validate_immediate_abort_cargo_opt_in(config)
        cargo_manifest.write_text(
            "[package]\nname='ronsole'\nversion='0.0.0'\n", encoding="utf-8"
        )
        _expect_pgo_error(
            lambda: validate_immediate_abort_cargo_opt_in(config),
            "missing panic-immediate-abort Cargo opt-in was accepted",
        )
        try:
            run_pipeline(replace(config, build_only=True), runner=_SelfTestRunner())
        except PgoError as error:
            if "panic-immediate-abort" not in str(error):
                raise PgoError(
                    "missing immediate-abort opt-in was not rejected in preflight"
                ) from error
        else:
            raise PgoError("missing immediate-abort opt-in passed PGO preflight")
        cargo_manifest.write_text(
            'cargo-features = ["unrelated-feature"]\n\n'
            "[package]\nname='ronsole'\nversion='0.0.0'\n",
            encoding="utf-8",
        )
        _expect_pgo_error(
            lambda: validate_immediate_abort_cargo_opt_in(config),
            "unrelated Cargo feature satisfied immediate-abort opt-in",
        )
        cargo_manifest.write_text(valid_cargo_manifest, encoding="utf-8")

        command = cargo_build_command(config)
        required_tail = ["--target", config.target, "--release", "--bin", "ronsole"]
        if (
            command[-5:] != required_tail
            or "--offline" not in command
            or "build-std=core,alloc,std,panic_abort,test" not in command
        ):
            raise PgoError("build command construction self-test failed")

        generate_environment = build_environment(
            config,
            target_dir=paths.generate_target_dir,
            pgo_flags=[f"-Cprofile-generate={paths.profile_dir}"],
            panic_strategy="abort",
        )
        generate_flags = generate_environment.get("CARGO_ENCODED_RUSTFLAGS", "").split("\x1f")
        if f"-Cprofile-generate={paths.profile_dir}" not in generate_flags:
            raise PgoError("profile-generate flag self-test failed")
        if generate_environment.get("CARGO_PROFILE_RELEASE_PANIC") != "abort":
            raise PgoError("instrumented panic=abort self-test failed")

        use_environment = build_environment(
            config,
            target_dir=paths.use_target_dir,
            pgo_flags=[
                f"-Cprofile-use={paths.merged_profile}",
                "-Cllvm-args=-pgo-warn-missing-function",
            ],
            panic_strategy="immediate-abort",
        )
        use_flags = use_environment.get("CARGO_ENCODED_RUSTFLAGS", "").split("\x1f")
        if f"-Cprofile-use={paths.merged_profile}" not in use_flags:
            raise PgoError("profile-use flag self-test failed")
        if "-Cllvm-args=-pgo-warn-missing-function" not in use_flags:
            raise PgoError("PGO missing-function warning self-test failed")
        if use_environment.get("CARGO_PROFILE_RELEASE_PANIC") != "immediate-abort":
            raise PgoError("final immediate-abort self-test failed")

        parsed_environment = parse_env(["A=1", "B=two=parts"])
        if parsed_environment != {"A": "1", "B": "two=parts"}:
            raise PgoError("environment parsing self-test failed")
        _expect_pgo_error(
            lambda: parse_env(["BROKEN"]), "invalid environment was accepted"
        )

        original_runtime = root / "original-runtime"
        original_runtime.mkdir()
        socket_path = original_runtime / "wayland-self-test"
        wayland_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            wayland_socket.bind(str(socket_path))
            relative_environment = {
                "WAYLAND_DISPLAY": socket_path.name,
                "XDG_RUNTIME_DIR": str(original_runtime),
            }
            resolved_relative = resolve_wayland_socket(relative_environment)
            resolved_absolute = resolve_wayland_socket(
                {"WAYLAND_DISPLAY": str(socket_path)}
            )
            if resolved_relative != socket_path or resolved_absolute != socket_path:
                raise PgoError("absolute Wayland socket resolution self-test failed")

            long_root = (
                root
                / ("long-project-root-" + "x" * 96)
                / ("nested-project-root-" + "y" * 96)
            )
            long_config = replace(config, root=long_root)
            long_paths = paths_for(long_config)
            legacy_socket_path = (
                long_paths.workspace / "runtime" / "ronsole" / "instance.sock"
            )
            if len(os.fsencode(legacy_socket_path)) < 200:
                raise PgoError("long project path regression fixture is too short")

            create_training_workspace(paths)
            first_runtime: Path | None = None
            with private_training_runtime() as private_runtime_name:
                first_runtime = Path(private_runtime_name)
                isolated = isolated_training_environment(
                    long_config,
                    long_paths,
                    wayland_socket=resolved_absolute,
                    profile_dir=long_paths.profile_dir,
                    private_runtime=first_runtime,
                )
                private_runtime = Path(isolated["XDG_RUNTIME_DIR"])
                if private_runtime != first_runtime:
                    raise PgoError("private XDG_RUNTIME_DIR ownership self-test failed")
                if stat.S_IMODE(private_runtime.stat().st_mode) != 0o700:
                    raise PgoError("private XDG_RUNTIME_DIR mode self-test failed")
                try:
                    private_runtime.relative_to(long_paths.workspace)
                except ValueError:
                    pass
                else:
                    raise PgoError(
                        "private XDG_RUNTIME_DIR still depends on the project path"
                    )
                if isolated.get("WAYLAND_DISPLAY") != str(socket_path):
                    raise PgoError("isolated absolute WAYLAND_DISPLAY self-test failed")
                if isolated.get("HOME") != str(long_paths.workspace / "home"):
                    raise PgoError("isolated HOME self-test failed")
                if isolated.get("SHELL") != "/bin/sh":
                    raise PgoError("deterministic SHELL self-test failed")

                ronsole_runtime = private_runtime / "ronsole"
                ronsole_runtime.mkdir(mode=0o700)
                instance_socket_path = ronsole_runtime / "instance.sock"
                instance_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                try:
                    instance_socket.bind(str(instance_socket_path))
                finally:
                    instance_socket.close()
                    instance_socket_path.unlink(missing_ok=True)

            if first_runtime is None or first_runtime.exists():
                raise PgoError("private XDG_RUNTIME_DIR cleanup self-test failed")
            with private_training_runtime() as second_runtime_name:
                second_runtime = Path(second_runtime_name)
                if second_runtime == first_runtime:
                    raise PgoError("private XDG_RUNTIME_DIR uniqueness self-test failed")
            if second_runtime.exists():
                raise PgoError("second private XDG_RUNTIME_DIR cleanup self-test failed")
        finally:
            wayland_socket.close()

        fixture = paths.workspace / "terminal_fixture.sh"
        fixture_text = fixture.read_text(encoding="utf-8")
        if not fixture.is_file() or not os.access(fixture, os.X_OK):
            raise PgoError("terminal fixture generation self-test failed")
        if any(f"  {phase})" not in fixture_text for phase in FIXTURE_PHASES):
            raise PgoError("terminal fixture phases self-test failed")
        alt_start = fixture_text.index("  alternate-screen)")
        alt_end = fixture_text.index("  process-tree)", alt_start)
        alt_fixture = fixture_text[alt_start:alt_end]
        alt_enter = alt_fixture.index("\\033[?1049h")
        alt_update_2 = alt_fixture.index("alt-screen frame 2")
        alt_update_3 = alt_fixture.index("alt-screen frame 3 updated")
        alt_exit = alt_fixture.index("\\033[?1049l")
        wait_1 = alt_fixture.index("sleep 0.25")
        wait_2 = alt_fixture.index("sleep 0.25", wait_1 + 1)
        wait_3 = alt_fixture.index("sleep 0.25", wait_2 + 1)
        if not (
            alt_enter
            < wait_1
            < alt_update_2
            < wait_2
            < alt_update_3
            < wait_3
            < alt_exit
        ):
            raise PgoError("alternate-screen update/wait ordering self-test failed")
        if alt_fixture.count("sleep 0.25") != 3:
            raise PgoError("alternate-screen bounded wait count self-test failed")
        if any(tool in alt_fixture.lower() for tool in ("htop", "btop", "fzf")):
            raise PgoError("alternate-screen fixture external dependency self-test failed")

        child_command = training_command(
            config, paths, Path("/tmp/ronsole-instrumented")
        )
        expected_contract = [
            "--pgo-train",
            "--pgo-workspace",
            str(paths.workspace.resolve()),
            "--pgo-report",
            str(paths.report_path.resolve()),
            "--pgo-timeout-seconds",
            str(config.timeout_seconds),
        ]
        if child_command[1:] != expected_contract:
            raise PgoError("Ronsole PGO command-line contract self-test failed")

        valid_report = {
            "status": "ok",
            "scenario_version": SCENARIO_VERSION,
            "completed_steps": list(EXPECTED_COMPLETED_STEPS),
            "skipped_steps": [],
            "duration_ms": 123,
        }
        validate_training_report(valid_report)
        _expect_pgo_error(
            lambda: validate_training_report({**valid_report, "scenario_version": 2}),
            "scenario version mismatch was accepted",
        )
        _expect_pgo_error(
            lambda: validate_training_report({**valid_report, "completed_steps": []}),
            "empty completed_steps was accepted",
        )
        invalid_completed_steps = {
            "partial startup/finish": ["startup", "finish"],
            "garbage": ["garbage"],
            "missing mandatory step": [
                step for step in EXPECTED_COMPLETED_STEPS if step != "bulk-output"
            ],
            "swapped order": [
                EXPECTED_COMPLETED_STEPS[1],
                EXPECTED_COMPLETED_STEPS[0],
                *EXPECTED_COMPLETED_STEPS[2:],
            ],
            "duplicate step": [
                EXPECTED_COMPLETED_STEPS[0],
                EXPECTED_COMPLETED_STEPS[0],
                *EXPECTED_COMPLETED_STEPS[1:],
            ],
            "missing finish": list(EXPECTED_COMPLETED_STEPS[:-1]),
        }
        for description, completed_steps in invalid_completed_steps.items():
            _expect_pgo_error(
                lambda steps=completed_steps: validate_training_report(
                    {**valid_report, "completed_steps": steps}
                ),
                f"invalid completed_steps ({description}) was accepted",
            )
        _expect_pgo_error(
            lambda: validate_training_report(
                {
                    **valid_report,
                    "status": "error",
                    "failed_step": "bulk",
                    "error": "self-test failure",
                }
            ),
            "failed automation report was accepted",
        )

        baseline_fingerprint = source_fingerprint(root)
        (root / "target" / "pgo-profiles" / config.target).mkdir(
            parents=True, exist_ok=True
        )
        (root / "target" / "generated.log").write_text("ignored\n", encoding="utf-8")
        (root / ".git").mkdir()
        (root / ".git" / "index").write_text("ignored\n", encoding="utf-8")
        (root / "cache").mkdir()
        (root / "cache" / "report.json").write_text("ignored\n", encoding="utf-8")
        if source_fingerprint(root) != baseline_fingerprint:
            raise PgoError("source fingerprint exclusions self-test failed")

        runner = _SelfTestRunner()
        instrumented_executable = executable_path(
            paths.generate_target_dir, config.target, config.binary_name
        )
        instrumented_executable.parent.mkdir(parents=True, exist_ok=True)
        instrumented_executable.write_bytes(b"instrumented-self-test")
        write_instrumented_build_manifest(
            config, paths, runner, instrumented_executable
        )
        validate_instrumented_build(config, paths, runner)

        source.write_text("fn main() { println!(\"changed\"); }\n", encoding="utf-8")
        _expect_pgo_error(
            lambda: validate_instrumented_build(config, paths, runner),
            "source changes after pgo-gen were accepted",
        )
        source.write_text("fn main() {}\n", encoding="utf-8")

        makefile = root / "Makefile"
        original_makefile = makefile.read_text(encoding="utf-8")
        makefile.write_text(original_makefile + "# changed build contract\n", encoding="utf-8")
        _expect_pgo_error(
            lambda: validate_instrumented_build(config, paths, runner),
            "Makefile changes after pgo-gen were accepted",
        )
        makefile.write_text(original_makefile, encoding="utf-8")

        changed_flags = replace(config, rustflags=config.rustflags + " -C opt-level=2")
        _expect_pgo_error(
            lambda: validate_instrumented_build(changed_flags, paths, runner),
            "instrumented build flag changes were accepted",
        )
        changed_profile_environment = replace(
            config,
            cargo_env={
                **config.cargo_env,
                "CARGO_PROFILE_RELEASE_OPT_LEVEL": "2",
            },
        )
        _expect_pgo_error(
            lambda: validate_instrumented_build(
                changed_profile_environment, paths, runner
            ),
            "cargo profile environment changes were accepted",
        )
        changed_toolchain = _SelfTestRunner(
            rustc="rustc 1.99.1-nightly (different-self-test)"
        )
        _expect_pgo_error(
            lambda: validate_instrumented_build(config, paths, changed_toolchain),
            "toolchain identity changes were accepted",
        )
        changed_target = replace(config, target="aarch64-unknown-linux-gnu")
        _expect_pgo_error(
            lambda: validate_instrumented_build(
                changed_target, paths_for(changed_target), runner
            ),
            "target changes were accepted",
        )

        instrumented_executable.write_bytes(b"tampered-instrumented")
        _expect_pgo_error(
            lambda: validate_instrumented_build(config, paths, runner),
            "instrumented executable hash changes were accepted",
        )
        instrumented_executable.write_bytes(b"instrumented-self-test")
        validate_instrumented_build(config, paths, runner)

        paths.report_path.parent.mkdir(parents=True, exist_ok=True)
        paths.report_path.write_text(
            json.dumps(valid_report, sort_keys=True) + "\n", encoding="utf-8"
        )
        raw_profile = paths.profile_dir / "self-test.profraw"
        raw_profile.write_bytes(b"raw")
        write_training_manifest(paths, valid_report, [raw_profile])
        validated_report, validated_profiles = validate_training_for_merge(
            config, paths, runner
        )
        if validated_report != valid_report or validated_profiles != [raw_profile]:
            raise PgoError("fresh generation/training provenance self-test failed")

        generation_manifest_bytes = paths.instrumented_manifest_path.read_bytes()
        paths.instrumented_manifest_path.unlink()
        _expect_pgo_error(
            lambda: validate_training_for_merge(config, paths, runner),
            "pgo-merge accepted missing generation provenance",
        )
        paths.instrumented_manifest_path.write_bytes(generation_manifest_bytes)
        validate_training_for_merge(config, paths, runner)

        raw_profile.write_bytes(b"tampered-raw-profile")
        _expect_pgo_error(
            lambda: validate_training_provenance(paths),
            "training provenance accepted modified raw profile bytes",
        )
        raw_profile.write_bytes(b"raw")
        validate_training_provenance(paths)

        paths.merged_profile.write_bytes(b"self-test-profdata")
        paths.summary_path.write_text("Functions shown: 1\n", encoding="utf-8")

        source.write_text("fn main() { println!(\"stale-generation\"); }\n", encoding="utf-8")
        _expect_pgo_error(
            lambda: write_manifest(config, paths, runner, [raw_profile], valid_report),
            "final manifest was minted from stale generation provenance",
        )
        source.write_text("fn main() {}\n", encoding="utf-8")

        write_manifest(config, paths, runner, [raw_profile], valid_report)
        validate_profile(config, paths, runner)

        source.write_text("fn main() { println!(\"changed\"); }\n", encoding="utf-8")
        _expect_pgo_error(
            lambda: validate_profile(config, paths, runner),
            "stale source profile was accepted",
        )
        source.write_text("fn main() {}\n", encoding="utf-8")
        write_manifest(config, paths, runner, [raw_profile], valid_report)
        paths.merged_profile.write_bytes(b"tampered-profdata")
        _expect_pgo_error(
            lambda: validate_profile(config, paths, runner),
            "profile hash mismatch was accepted",
        )

        parsed = parse_args(
            [
                "--target",
                config.target,
                "--mode",
                "fresh",
                "--train-only",
                "--timeout-seconds",
                "120",
            ]
        )
        if not parsed.train_only or parsed.mode != "fresh" or parsed.timeout_seconds != 120:
            raise PgoError("pipeline command-line mode self-test failed")

    print("[ronsole-pgo] self-test passed", flush=True)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    if args.self_test:
        self_test()
        return 0
    if not args.target:
        raise PgoError("--target is required unless --self-test is used")
    config = PgoConfig(
        root=ROOT,
        target=args.target,
        binary_name=args.binary_name,
        mode=args.mode,
        rustflags=args.rustflags,
        build_std_flags=args.build_std_flags,
        timeout_seconds=args.timeout_seconds,
        cargo_env=parse_env(args.env),
        train_only=args.train_only,
        run_only=args.run_only,
        run_executable=args.run_executable,
        build_only=args.build_only,
        merge_only=args.merge_only,
        verbose=not args.quiet,
    )
    executable = run_pipeline(config)
    if executable is not None:
        print(f"[ronsole-pgo] executable: {executable}", flush=True)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (PgoError, subprocess.CalledProcessError, OSError) as error:
        print(f"[ronsole-pgo] ERROR: {error}", file=sys.stderr, flush=True)
        raise SystemExit(1)
