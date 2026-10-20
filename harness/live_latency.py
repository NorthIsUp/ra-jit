#!/usr/bin/env python3
"""Save-to-running latency of live edits (macOS on AArch64), step by step.

usage: live_latency.py [cargo|socket ...]   (RA_JIT_BIN names the ra-jit built; cargo-ra-jit
                                             and ra-jit-rustc are found next to it)

harness/live (a program that polls ra-jit-live every millisecond) is built and started as
live_e2e.py does, then takes LATENCY_EDITS edits (default 10) in each mode named:

  cargo   the file is written and `cargo ra-jit build` asks the daemon for the build, as the
          demo's launcher did on each save (its 200 ms mtime poll, 100 ms on average, comes
          on top and is not in these numbers)
  socket  the text goes straight to the daemon (`edit` line on its socket, what the demo's
          editor sends on each keystroke); the file is written after, off the clock

Each edit is timed from just before it is written or sent (t0) to the program having the new
code in place, and split by the epoch stamps the worker (`request:`, `live: ... at_us=`) and
the program (`outcome: ... at_us=`) print:

  to-worker   t0 to the worker reading the request (cargo and the daemon's hop)
  worker      the request to the patch left for the program (`timeline-us:` splits it)
  delivery    the patch file written to the program's thread reading it
  wait        read to applied: the program's next poll (1 ms here, a frame in the demo)
  apply       mapping the patch and writing the jumps
  total       t0 to applied, what the demo shows as "edit to running"
  reply       t0 to the client's answer (after the on-disk splice)

The medians go to stdout and, as a table, to $GITHUB_STEP_SUMMARY when it is set; every edit's
numbers go to $LATENCY_JSON when it is set. Python 3.13, stdlib only.
"""

import asyncio
import json
import os
import re
import shutil
import statistics
import sys
import tempfile
import time
from pathlib import Path

from live_e2e import ROOT, HERE, StepError, build, daemon_home, edit, pump, show_daemon_log, wait_for

EDITS = int(os.environ.get("LATENCY_EDITS", "10"))
REPLY_TIMEOUT = 60
COLUMNS = ["to-worker", "worker", "delivery", "wait", "apply", "total", "reply"]


def now_us() -> int:
    return time.time_ns() // 1000


def escape(text: str) -> str:
    return text.replace("\\", "\\\\").replace("\n", "\\n").replace("\t", "\\t").replace("\r", "\\r")


async def send_edit(path: Path, text: str) -> str:
    """What the demo's editor does on each edit: the whole text, on one line, to the daemon."""
    socket = daemon_home() / "daemon.sock"
    try:
        async with asyncio.timeout(REPLY_TIMEOUT):
            reader, writer = await asyncio.open_unix_connection(str(socket))
            writer.write(f"edit\t{path}\t{escape(text)}\n".encode())
            await writer.drain()
            reply = (await reader.readline()).decode(errors="replace").strip()
            writer.close()
    except TimeoutError:
        raise StepError(f"send edit: no reply from {socket} in {REPLY_TIMEOUT}s") from None
    if not reply.startswith("ok"):
        raise StepError(f"send edit: the daemon said {reply!r}")
    return reply


class Logs:
    """The worker logs, read from where the last edit left off."""

    def __init__(self) -> None:
        self.offsets: dict[Path, int] = {}
        self.skip()

    def files(self) -> list[Path]:
        return sorted(daemon_home().glob("workers/*/log"))

    def skip(self) -> None:
        for log in self.files():
            self.offsets[log] = log.stat().st_size

    def new(self) -> list[str]:
        lines = []
        for log in self.files():
            with log.open("rb") as f:
                f.seek(self.offsets.get(log, 0))
                data = f.read()
            self.offsets[log] = self.offsets.get(log, 0) + len(data)
            lines += data.decode(errors="replace").splitlines()
        return lines


def at_us(line: str) -> int | None:
    found = re.search(r"at_us=(\d+)", line)
    return int(found.group(1)) if found else None


def duration_ms(text: str) -> float:
    """A Rust `Duration` as Debug prints it: 1.2ms, 830.0µs, 2.1s."""
    found = re.match(r"([\d.]+)(ns|µs|us|ms|s)", text)
    if not found:
        return float("nan")
    scale = {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1.0, "s": 1e3}[found.group(2)]
    return float(found.group(1)) * scale


def breakdown(t0: int, reply: int, applied_line: str, worker: list[str]) -> dict[str, float]:
    requests = [it for line in worker if line.startswith("request:") and (it := at_us(line)) and it >= t0]
    published = [it for line in worker if line.startswith("live: edit") and (it := at_us(line)) and it >= t0]
    if not requests or not published:
        raise StepError(f"timing: no `request:` or `live: edit` stamp in the worker log since the edit:\n  " + "\n  ".join(worker[-30:]))
    found = re.search(r" in (\S+) after waiting (\S+) at_us=(\d+)", applied_line)
    if not found:
        raise StepError(f"timing: the program's outcome has no times: {applied_line}")
    took, waited, applied = duration_ms(found.group(1)), duration_ms(found.group(2)), int(found.group(3))
    request, publish = requests[0], published[-1]
    ms = lambda us: us / 1e3
    row = {
        "to-worker": ms(request - t0),
        "worker": ms(publish - request),
        "delivery": ms(applied - publish) - waited - took,
        "wait": waited,
        "apply": took,
        "total": ms(applied - t0),
        "reply": ms(reply - t0),
    }
    for line in worker:
        if line.startswith("timeline-us:"):
            for step in line.split()[1:]:
                name, _, value = step.partition("=")
                row[f"worker.{name}"] = int(value) / 1e3
        elif line.startswith("edit-to-binary:"):
            row["worker.edit-to-binary"] = duration_ms(line.split()[1])
        elif line.startswith("link:") and "spliced" in line:
            row["worker.link"] = duration_ms(line.split()[1])
    return row


def summarize(mode: str, rows: list[dict[str, float]]) -> str:
    keys = COLUMNS + sorted({k for row in rows for k in row if k not in COLUMNS})
    out = [f"### {mode}: {len(rows)} edits, ms", "", "| step | median | p90 | min | max |", "|---|---|---|---|---|"]
    for key in keys:
        values = sorted(row[key] for row in rows if key in row)
        if not values:
            continue
        p90 = values[min(len(values) - 1, int(0.9 * len(values)))]
        out.append(f"| {key} | {statistics.median(values):.2f} | {p90:.2f} | {values[0]:.2f} | {values[-1]:.2f} |")
    return "\n".join(out)


async def main(modes: list[str]) -> None:
    bin_dir = Path(os.environ["RA_JIT_BIN"]).resolve().parent
    work = Path(tempfile.mkdtemp(prefix="ra-jit-live-latency-"))
    probe = None
    results: dict[str, list[dict[str, float]]] = {}
    try:
        shutil.copytree(ROOT / "crates" / "ra-jit-live", work / "crates" / "ra-jit-live")
        shutil.copytree(HERE / "live", work / "harness" / "live", ignore=shutil.ignore_patterns("target"))
        project = work / "harness" / "live"
        source = project / "src" / "main.rs"
        env = dict(
            os.environ,
            PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            CARGO_TARGET_DIR=str(work / "target"),
            RA_JIT_STATS="1",
        )
        env.pop("CARGO_INCREMENTAL", None)

        await build("first build", project, env)
        for _ in range(120):
            if "[ra-jit]" in await build("wait for the daemon", project, env):
                break
            await asyncio.sleep(0.5)
        else:
            raise StepError("wait for the daemon: no build said `[ra-jit]` in 120 tries")
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

        logs = Logs()
        value = 2
        for mode in modes:
            rows = []
            # The first edit of a mode is a warm-up: not counted.
            for nth in range(EDITS + 1):
                text = source.read_text()
                needle = f"fn answer() -> u64 {{\n    {value}\n}}"
                if needle not in text:
                    raise StepError(f"{mode} edit {nth}: `{needle}` is not in {source}")
                new = text.replace(needle, f"fn answer() -> u64 {{\n    {value + 1}\n}}")
                logs.skip()
                t0 = now_us()
                if mode == "cargo":
                    source.write_text(new)
                    out = await build(f"{mode} edit {nth}", project, env)
                    said = out.strip().splitlines()[-1] if out.strip() else ""
                elif mode == "socket":
                    said = await send_edit(source, new)
                    source.write_text(new)
                else:
                    raise StepError(f"unknown mode {mode!r}: cargo or socket")
                reply = now_us()
                value += 1
                applied = await wait_for(f"{mode} edit {nth} applied", lines, "outcome:", log)
                if "applied" not in applied:
                    raise StepError(f"{mode} edit {nth}: the program did not take the patch: {applied}")
                await wait_for(f"{mode} edit {nth} runs", lines, f"answer {value}", log)
                # The reply came after the worker logged the whole edit.
                row = breakdown(t0, reply, applied, logs.new())
                print(f"{mode} {nth}: " + " ".join(f"{k}={row[k]:.2f}" for k in COLUMNS) + f"  ({said})")
                if nth:
                    rows.append(row)
            results[mode] = rows
        pumper.cancel()
    finally:
        if probe is not None and probe.returncode is None:
            probe.terminate()
            await probe.wait()
        shutil.rmtree(work, ignore_errors=True)

    tables = "\n\n".join(summarize(mode, rows) for mode, rows in results.items())
    print(tables)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as f:
            f.write(tables + "\n")
    if out := os.environ.get("LATENCY_JSON"):
        Path(out).write_text(json.dumps(results, indent=1))


if __name__ == "__main__":
    try:
        asyncio.run(main(sys.argv[1:] or ["cargo"]))
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        show_daemon_log()
        sys.exit(1)
