#!/usr/bin/env python3
"""Live edits end to end (macOS on AArch64): edit a function of a running program, and the
running program must run the new code, applied by ra-jit-live from the daemon's patch.

usage: live_e2e.py          (RA_JIT_BIN names the ra-jit built; cargo-ra-jit and ra-jit-rustc
                             are found next to it)

A copy of harness/live (a program that polls ra-jit-live and prints `answer()`) is built by the
daemon until the daemon serves the build, edited once (the first edit after a daemon starts
writes the whole object again), started, and edited again LIVE_EDITS times (default 5). Each edit has to arrive as a
patch the program applies: `outcome: edit N applied` and the new answer on its output.

Python 3.13, stdlib only. Every step has a timeout; a failure names the step.
"""

import asyncio
import os
import shutil
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
BUILD_TIMEOUT = 900
APPLY_TIMEOUT = 60
# How many edits the running program takes one after another.
LIVE_EDITS = int(os.environ.get("LIVE_EDITS", "5"))


class StepError(Exception):
    pass


async def build(step: str, cwd: Path, env: dict[str, str]) -> str:
    cmd = ["cargo-ra-jit", "ra-jit", "build", "--locked"]
    try:
        async with asyncio.timeout(BUILD_TIMEOUT):
            proc = await asyncio.create_subprocess_exec(
                *cmd, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
            )
            out, err = await proc.communicate()
    except TimeoutError:
        raise StepError(f"{step}: timed out after {BUILD_TIMEOUT}s\n  command: {' '.join(cmd)}") from None
    text = (out + err).decode(errors="replace")
    if proc.returncode != 0:
        tail = "\n".join(text.strip().splitlines()[-30:])
        raise StepError(f"{step}: exit code {proc.returncode}\n  command: {' '.join(cmd)}\n  output tail:\n{tail}")
    return text


def edit(source: Path, old: int, new: int) -> None:
    text = source.read_text()
    needle = f"fn answer() -> u64 {{\n    {old}\n}}"
    if needle not in text:
        raise StepError(f"edit: `{needle}` is not in {source}")
    source.write_text(text.replace(needle, f"fn answer() -> u64 {{\n    {new}\n}}"))


async def wait_for(step: str, lines: asyncio.Queue[str], wanted: str, log: list[str]) -> str:
    try:
        async with asyncio.timeout(APPLY_TIMEOUT):
            while True:
                line = await lines.get()
                log.append(line)
                if wanted in line:
                    return line
    except TimeoutError:
        raise StepError(f"{step}: no `{wanted}` on the program's output in {APPLY_TIMEOUT}s; it said:\n  " + "\n  ".join(log[-20:])) from None


async def pump(stream: asyncio.StreamReader, lines: asyncio.Queue[str]) -> None:
    while line := await stream.readline():
        await lines.put(line.decode(errors="replace").rstrip())


async def main() -> None:
    bin_dir = Path(os.environ["RA_JIT_BIN"]).resolve().parent
    work = Path(tempfile.mkdtemp(prefix="ra-jit-live-e2e-"))
    probe = None
    try:
        shutil.copytree(ROOT / "crates" / "ra-jit-live", work / "crates" / "ra-jit-live")
        shutil.copytree(HERE / "live", work / "harness" / "live", ignore=shutil.ignore_patterns("target"))
        project = work / "harness" / "live"
        source = project / "src" / "main.rs"
        env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", CARGO_TARGET_DIR=str(work / "target"))
        env.pop("CARGO_INCREMENTAL", None)

        await build("first build", project, env)
        # Until the daemon has loaded the workspace a build is cargo's; one it served says `[ra-jit]`.
        for attempt in range(120):
            if "[ra-jit]" in await build("wait for the daemon", project, env):
                break
            await asyncio.sleep(0.5)
        else:
            raise StepError("wait for the daemon: no build said `[ra-jit]` in 120 tries")
        print("daemon is serving builds")

        edit(source, 1, 2)
        await build("edit before start", project, env)
        probe = await asyncio.create_subprocess_exec(
            str(work / "target" / "debug" / "liveprobe"), cwd=project, env=env,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
        )
        lines: asyncio.Queue[str] = asyncio.Queue()
        pumper = asyncio.create_task(pump(probe.stdout, lines))
        log: list[str] = []
        await wait_for("program start", lines, "ready", log)
        await wait_for("program runs the edited source", lines, "answer 2", log)
        print("program started with answer 2")

        # Edit after edit, as the demo's editor does: each has to arrive as a patch the running
        # program applies, none may link the program again.
        for new in range(3, 3 + LIVE_EDITS):
            edit(source, new - 1, new)
            out = await build(f"edit {new - 2} while running", project, env)
            print("build:", out.strip().splitlines()[-1] if out.strip() else "")
            applied = await wait_for(f"patch {new - 2} applied", lines, "outcome:", log)
            print(applied)
            if "applied" not in applied:
                raise StepError(f"patch {new - 2} applied: the program did not take the patch: {applied}")
            await wait_for(f"new code of edit {new - 2} runs", lines, f"answer {new}", log)
        chain = " -> ".join(str(it) for it in range(1, 3 + LIVE_EDITS))
        print(f"live edits: answer {chain} in a running program, each applied by ra-jit-live")
        pumper.cancel()
    finally:
        if probe is not None and probe.returncode is None:
            probe.terminate()
            await probe.wait()
        shutil.rmtree(work, ignore_errors=True)


def daemon_home() -> Path:
    """Where the daemon keeps its socket and log, as `cargo::daemon::home` finds it."""
    if os.environ.get("RA_JIT_HOME"):
        return Path(os.environ["RA_JIT_HOME"])
    if os.environ.get("XDG_RUNTIME_DIR"):
        return Path(os.environ["XDG_RUNTIME_DIR"]) / "ra-jit"
    return Path.home() / ".cache" / "ra-jit"


def show_daemon_log(lines: int = 80) -> None:
    """The daemon's log, and those of its workers: a worker that died says why in its own."""
    home = daemon_home()
    logs = [home / "daemon.log", *sorted(home.glob("workers/*/log")), *sorted(home.glob("spares/*/log"))]
    for log in logs:
        try:
            tail = log.read_text(errors="replace").splitlines()[-lines:]
        except OSError as why:
            print(f"no log at {log}: {why}", file=sys.stderr)
            continue
        print(f"--- the last {len(tail)} lines of {log}", file=sys.stderr)
        print("\n".join(tail), file=sys.stderr)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        show_daemon_log()
        sys.exit(1)
