#!/usr/bin/env python3
"""Save-to-running latency of live edits (macOS on AArch64), step by step.

usage: live_latency.py [--program probe|life] [cargo|socket|burst ...]
       (RA_JIT_BIN names the ra-jit built; cargo-ra-jit and ra-jit-rustc are found next to it)

The program is harness/live (`probe`, the default: a few functions and ra-jit-live) or the Game
of Life demo (`life`: demo/life with eframe, a 20 MB debug executable, run without its window
with RA_JIT_LIFE_HEADLESS=1). Either applies each patch as it arrives, as the demo's window does.
It is built and started as live_e2e.py does, then takes LATENCY_EDITS edits (default 10) in each
mode named:

  cargo   the file is written and `cargo ra-jit build` asks the daemon for the build, as the
          demo's launcher did on each save (its 200 ms mtime poll, 100 ms on average, comes
          on top and is not in these numbers)
  socket  the text goes straight to the daemon (`edit` line on its socket, what the demo's
          editor sends on each keystroke); the file is written after, off the clock. One edit
          at a time, each after the last is running: an isolated edit
  burst   typing: LATENCY_BURSTS bursts (default 3) of LATENCY_KEYS keystrokes (default 20),
          30 to 100 ms apart, each a valid edit of the function. They go to the daemon as the
          demo's editor sends them: one edit on the wire at a time, the newest text next, the
          file written after each answer. Each keystroke is timed from when it was typed to
          the program running code at least as new as it (the program's `answer N at_us=`);
          `queued` is the part before the edit that has it was sent

Each edit of cargo and socket is timed from just before it is written or sent (t0) to the
program having the new code in place, and split by the epoch stamps the worker (`request:`,
`live: ... at_us=`) and the program (`outcome: ... at_us=`) print:

  to-worker   t0 to the worker reading the request (cargo and the daemon's hop)
  worker      the request to the patch left for the program (`timeline-us:` splits it)
  delivery    the patch file written to the program's thread reading it
  wait        read to applied: the program's thread taking it up
  apply       mapping the patch and writing the jumps
  total       t0 to applied, what the demo shows as "edit to running" (the old in-process
              prototype timed the same span: from just before the edit left the editor to
              the new code in place, with no frame in between)
  reply       t0 to the client's answer

The medians go to stdout and, as a table, to $GITHUB_STEP_SUMMARY when it is set; every edit's
numbers go to $LATENCY_JSON when it is set. Python 3.13, stdlib only.
"""

import asyncio
import json
import os
import random
import re
import shutil
import statistics
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

from live_e2e import ROOT, HERE, StepError, build, daemon_home, pump, show_daemon_log, wait_for

EDITS = int(os.environ.get("LATENCY_EDITS", "10"))
KEYS = int(os.environ.get("LATENCY_KEYS", "20"))
BURSTS = int(os.environ.get("LATENCY_BURSTS", "3"))
REPLY_TIMEOUT = 60
COLUMNS = ["to-worker", "worker", "delivery", "wait", "apply", "total", "reply"]


@dataclass
class Program:
    """A program to edit while it runs: where its crate is, and the function edited."""

    name: str
    project: Path
    binary: str
    function: str
    value: int
    env: dict[str, str]

    def needle(self, value: int) -> str:
        return f"fn {self.function}() -> u64 {{\n    {value}\n}}"

    def edited(self, text: str, old: int, new: int) -> str:
        if self.needle(old) not in text:
            raise StepError(f"edit: `{self.needle(old)}` is not in {self.source}")
        return text.replace(self.needle(old), self.needle(new))

    @property
    def source(self) -> Path:
        return self.project / "src" / "main.rs"


def copy_program(name: str, work: Path) -> Program:
    shutil.copytree(ROOT / "crates" / "ra-jit-live", work / "crates" / "ra-jit-live")
    ignore = shutil.ignore_patterns("target")
    match name:
        case "probe":
            shutil.copytree(HERE / "live", work / "harness" / "live", ignore=ignore)
            return Program(name, work / "harness" / "live", "liveprobe", "answer", 1, {})
        case "life":
            shutil.copytree(ROOT / "demo" / "life", work / "demo" / "life", ignore=ignore)
            env = {"RA_JIT_LIFE_HEADLESS": "1"}
            return Program(name, work / "demo" / "life", "life", "step_ms", 80, env)
    raise StepError(f"unknown program {name!r}: probe or life")


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
        # A worker that was a spare, started ahead of the checkout, keeps the spare's log.
        home = daemon_home()
        return sorted([*home.glob("workers/*/log"), *home.glob("spares/*/log")])

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


def worker_steps(worker: list[str]) -> dict[str, list[float]]:
    """The steps the worker's log times, each with every value it had in `worker`."""
    steps: dict[str, list[float]] = {}
    for line in worker:
        if line.startswith(("timeline-us:", "apply-us:", "splice-us:")):
            prefix = {"t": "worker.", "a": "worker.apply.", "s": "worker.splice."}[line[0]]
            for step in line.split()[1:]:
                name, _, value = step.partition("=")
                if name != "bytes":
                    steps.setdefault(f"{prefix}{name}", []).append(int(value) / 1e3)
        elif line.startswith("live: edit") and (took_us := re.search(r"took_us=(\d+)", line)):
            # Laying the patch out for the program and leaving it there.
            steps.setdefault("worker.publish", []).append(int(took_us.group(1)) / 1e3)
        elif line.startswith("edit-to-binary:"):
            steps.setdefault("worker.edit-to-binary", []).append(duration_ms(line.split()[1]))
        elif line.startswith("link:") and "spliced" in line:
            steps.setdefault("worker.link", []).append(duration_ms(line.split()[1]))
    return steps


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
    # One edit, one build: the last value of each step is this edit's.
    row.update({name: values[-1] for name, values in worker_steps(worker).items()})
    return row


def summarize(title: str, rows: list[dict[str, float]], columns: list[str] = COLUMNS) -> str:
    keys = columns + sorted({k for row in rows for k in row if k not in columns})
    out = [f"### {title}, ms", "", "| step | median | p90 | min | max |", "|---|---|---|---|---|"]
    for key in keys:
        values = sorted(row[key] for row in rows if key in row)
        if not values:
            continue
        p90 = values[min(len(values) - 1, int(0.9 * len(values)))]
        out.append(f"| {key} | {statistics.median(values):.2f} | {p90:.2f} | {values[0]:.2f} | {values[-1]:.2f} |")
    return "\n".join(out)


class Running:
    """The program's output: each line in order, and when it ran each answer."""

    def __init__(self, lines: asyncio.Queue[str]) -> None:
        self.lines = lines
        self.log: list[str] = []

    async def wait_for(self, step: str, wanted: str) -> str:
        return await wait_for(step, self.lines, wanted, self.log)

    async def runs(self, step: str, value: int) -> int:
        """Waits for the program to run `value` (or newer) and says when it did."""
        pattern = re.compile(r"^answer (\d+) at_us=(\d+)")
        for line in self.log[self.seen :]:
            if (found := pattern.match(line)) and int(found.group(1)) >= value:
                return int(found.group(2))
        while True:
            line = await self.wait_for(step, "answer ")
            if (found := pattern.match(line)) and int(found.group(1)) >= value:
                return int(found.group(2))

    def mark(self) -> None:
        self.seen = len(self.log)

    seen = 0


async def one_by_one(mode: str, program: Program, running: Running, env: dict[str, str], logs: Logs) -> list[dict[str, float]]:
    rows = []
    # The first edit of a mode is a warm-up: not counted.
    for nth in range(EDITS + 1):
        new = program.edited(program.source.read_text(), program.value, program.value + 1)
        logs.skip()
        running.mark()
        t0 = now_us()
        if mode == "cargo":
            program.source.write_text(new)
            out = await build(f"{mode} edit {nth}", program.project, env)
            said = out.strip().splitlines()[-1] if out.strip() else ""
        else:
            said = await send_edit(program.source, new)
            program.source.write_text(new)
        reply = now_us()
        program.value += 1
        applied = await running.wait_for(f"{mode} edit {nth} applied", "outcome:")
        if "applied" not in applied:
            raise StepError(f"{mode} edit {nth}: the program did not take the patch: {applied}")
        # The new code can run before the apply thread says it was applied.
        await running.runs(f"{mode} edit {nth} runs", program.value)
        # Give the worker the moment it takes to log the rest of the build (the splice may
        # come after the answer).
        await asyncio.sleep(0.3)
        row = breakdown(t0, reply, applied, logs.new())
        print(f"{mode} {nth}: " + " ".join(f"{k}={row[k]:.2f}" for k in COLUMNS) + f"  ({said[-60:]})")
        if nth:
            rows.append(row)
    return rows


async def burst(program: Program, running: Running, logs: Logs, rng: random.Random, nth: int) -> tuple[list[dict[str, float]], dict[str, list[float]]]:
    """Types KEYS keystrokes and times each, as the demo's editor sends them."""
    typed: list[tuple[int, int]] = []  # (value, when typed)
    sent: list[tuple[int, int]] = []  # (value, when its edit was sent)
    replies: list[str] = []
    newest: list[tuple[str, int]] = []
    ready = asyncio.Event()
    done = False

    async def sender() -> None:
        # The demo's `send_as_typed`: one edit on the wire, then the newest text typed.
        while True:
            await ready.wait()
            ready.clear()
            if not newest:
                if done:
                    return
                continue
            text, value = newest.pop()
            newest.clear()
            sent.append((value, now_us()))
            replies.append(await send_edit(program.source, text))
            program.source.write_text(text)
            if newest:
                ready.set()

    logs.skip()
    running.mark()
    task = asyncio.create_task(sender())
    text = program.source.read_text()
    for _ in range(KEYS):
        await asyncio.sleep(rng.uniform(0.03, 0.1))
        text = program.edited(text, program.value, program.value + 1)
        program.value += 1
        typed.append((program.value, now_us()))
        newest[:] = [(text, program.value)]
        ready.set()
    done = True
    ready.set()
    async with asyncio.timeout(REPLY_TIMEOUT):
        await task
    last = await running.runs(f"burst {nth} runs the last keystroke", program.value)
    await asyncio.sleep(0.3)
    rows = []
    answers = [
        (int(found.group(1)), int(found.group(2)))
        for line in running.log[running.seen :]
        if (found := re.match(r"^answer (\d+) at_us=(\d+)", line))
    ]
    for value, at in typed:
        ran = min((when for seen, when in answers if seen >= value), default=last)
        send = min((when for has, when in sent if has >= value), default=at)
        rows.append({"total": (ran - at) / 1e3, "queued": (send - at) / 1e3})
    superseded = sum("superseded" in it for it in replies)
    applied = sum(1 for line in running.log[running.seen :] if line.startswith("outcome:") and "applied" in line)
    totals = sorted(row["total"] for row in rows)
    print(
        f"burst {nth}: {len(typed)} keystrokes, {len(sent)} edits sent ({superseded} superseded), {applied} patches applied; "
        f"keystroke to running p50 {statistics.median(totals):.2f} ms p90 {totals[int(0.9 * len(totals))]:.2f} ms max {totals[-1]:.2f} ms"
    )
    return rows, worker_steps(logs.new())


async def main(program_name: str, modes: list[str]) -> None:
    bin_dir = Path(os.environ["RA_JIT_BIN"]).resolve().parent
    work = Path(tempfile.mkdtemp(prefix="ra-jit-live-latency-"))
    probe = None
    results: dict[str, list[dict[str, float]]] = {}
    tables = []
    try:
        program = copy_program(program_name, work)
        env = dict(
            os.environ,
            PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            CARGO_TARGET_DIR=str(work / "target"),
            RA_JIT_STATS="1",
        )
        env.pop("CARGO_INCREMENTAL", None)

        await build("first build", program.project, env)
        for _ in range(120):
            if "[ra-jit]" in await build("wait for the daemon", program.project, env):
                break
            await asyncio.sleep(0.5)
        else:
            raise StepError("wait for the daemon: no build said `[ra-jit]` in 120 tries")
        program.source.write_text(program.edited(program.source.read_text(), program.value, program.value + 1))
        program.value += 1
        await build("edit before start", program.project, env)
        binary = work / "target" / "debug" / program.binary
        print(f"{program.name}: {binary.stat().st_size / 1e6:.1f} MB executable")
        probe = await asyncio.create_subprocess_exec(
            str(binary), cwd=program.project, env=dict(env, **program.env),
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
        )
        lines: asyncio.Queue[str] = asyncio.Queue()
        pumper = asyncio.create_task(pump(probe.stdout, lines))
        running = Running(lines)
        await running.wait_for("program start", "ready")
        await running.runs("program runs the edited source", program.value)

        logs = Logs()
        rng = random.Random(1)
        for mode in modes:
            match mode:
                case "cargo" | "socket":
                    rows = await one_by_one(mode, program, running, env, logs)
                    tables.append(summarize(f"{program.name}, {mode}: {len(rows)} edits one at a time", rows))
                case "burst":
                    rows, steps = [], {}
                    for nth in range(BURSTS):
                        got, worker = await burst(program, running, logs, rng, nth)
                        rows += got
                        for name, values in worker.items():
                            steps.setdefault(name, []).extend(values)
                        await asyncio.sleep(1)
                    title = f"{program.name}, burst: {len(rows)} keystrokes 30-100 ms apart, keystroke to running"
                    tables.append(summarize(title, rows, ["total", "queued"]))
                    # The worker's steps, per build of the bursts.
                    builds = [{name: values[i] for name, values in steps.items() if i < len(values)} for i in range(max(map(len, steps.values()), default=0))]
                    tables.append(summarize(f"{program.name}, burst: the worker, per build", builds, []))
                case _:
                    raise StepError(f"unknown mode {mode!r}: cargo, socket or burst")
            results[mode] = rows
        pumper.cancel()
    finally:
        if probe is not None and probe.returncode is None:
            probe.terminate()
            await probe.wait()
        shutil.rmtree(work, ignore_errors=True)

    text = "\n\n".join(tables)
    print(text)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as f:
            f.write(text + "\n")
    if out := os.environ.get("LATENCY_JSON"):
        Path(out).write_text(json.dumps(results, indent=1))


if __name__ == "__main__":
    args = sys.argv[1:]
    program = "probe"
    if args[:1] == ["--program"]:
        program, args = args[1], args[2:]
    try:
        asyncio.run(main(program, args or ["cargo"]))
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        show_daemon_log()
        sys.exit(1)
