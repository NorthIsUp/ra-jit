#!/usr/bin/env python3
"""Regenerate docs/demo.mp4 and docs/demo.webp: the README clip of the live Game of Life demo.

  python3 tools/record_demo.py [--no-build] [--webp-fps 30] [--webp-quality 40]

Runs `ra-jit life <fresh dir> --record docs/demo.mp4 --timeline <tsv>`: the egui window is rendered offscreen (a
software rasteriser, no OS window, no screen capture) at 1280x720, a fixed 60 fps, with the guest on a virtual clock.
The timeline below is the script of edits typed into the real editor; each one goes through the real hot-swap, so
the "edit to running" readout in the clip is measured. Then the mp4 is turned into a 960-wide looping animated WebP
with Pillow (this ffmpeg has no libwebp).

Python 3.12+, stdlib + Pillow. Every step has its own timeout; failures name the step, command, exit code and stderr tail.
"""

import argparse
import asyncio
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "docs"
BIN = ROOT / "target" / "release" / "ra-jit"

# (command, *fields); times are in 60 fps frames. `select` picks `target` inside the first `context` occurrence.
TIMELINE: list[tuple] = [
    ("wait", 120),
    # HighLife: B36/S23 instead of Conway's B3/S23.
    ("select", "if n == 3 { 1 }", "n == 3"),
    ("wait", 50),
    ("type", "n == 3 || n == 6"),
    ("wait", 170),
    # Repaint: young and old cells.
    ("select", "0xffd23f", "0xffd23f"),
    ("wait", 40),
    ("type", "0xff5d8f"),
    ("wait", 60),
    ("select", "0x3a86ff", "0x3a86ff"),
    ("wait", 40),
    ("type", "0xb04aff"),
    ("wait", 150),
    # Run faster: 25 steps/s -> 62.
    ("select", "const STEP_MS: i64 = 40;", "40"),
    ("wait", 40),
    ("type", "16"),
    ("wait", 200),
]


def esc(s: str) -> str:
    return s.replace("\\", "\\\\").replace("\n", "\\n").replace("\t", "\\t")


def timeline_tsv() -> str:
    return "".join("\t".join([cmd, *(esc(str(f)) for f in fields)]) + "\n" for cmd, *fields in TIMELINE)


class StepError(Exception):
    pass


async def run_step(step: str, cmd: list[str], timeout: float, stdin_data: bytes | None = None) -> bytes:
    async with asyncio.timeout(timeout):
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdin=asyncio.subprocess.PIPE if stdin_data is not None else None,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        out, err = await proc.communicate(stdin_data)
    if proc.returncode != 0:
        tail = err.decode(errors="replace")[-1500:]
        raise StepError(f"step {step!r} failed (exit {proc.returncode}): {' '.join(cmd)}\n{tail}")
    return out


async def make_webp(mp4: Path, out: Path, fps: int, quality: int) -> None:
    from PIL import Image

    w, h = 960, 540
    async with asyncio.timeout(600):
        proc = await asyncio.create_subprocess_exec(
            "ffmpeg", "-loglevel", "error", "-i", str(mp4), "-vf", f"fps={fps},scale={w}:{h}:flags=lanczos",
            "-f", "rawvideo", "-pix_fmt", "rgb24", "-",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        raw, err = await proc.communicate()
    if proc.returncode != 0:
        raise StepError(f"step 'webp frames' failed (exit {proc.returncode}):\n{err.decode(errors='replace')[-1500:]}")
    size = w * h * 3
    frames = [Image.frombytes("RGB", (w, h), raw[i : i + size]) for i in range(0, len(raw) - size + 1, size)]
    await asyncio.to_thread(
        frames[0].save, out, save_all=True, append_images=frames[1:], duration=round(1000 / fps),
        loop=0, quality=quality, method=4, minimize_size=True,
    )
    print(f"webp: {len(frames)} frames at {fps} fps, {out.stat().st_size / 1e6:.2f} MB")


async def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--webp-fps", type=int, default=30)
    ap.add_argument("--webp-quality", type=int, default=40)
    ap.add_argument("--no-record", action="store_true", help="only rebuild the webp from the existing mp4")
    a = ap.parse_args()

    if not a.no_record:
        if not a.no_build:
            await run_step("cargo build", ["cargo", "build", "--release"], 3600)
        mp4 = DOCS / "demo.mp4"
        with tempfile.TemporaryDirectory(prefix="ra-jit-record-") as tmp0:
            tmp = Path(tmp0).resolve()  # rust-analyzer needs the real path (macOS /var is a symlink)
            tl = Path(tmp) / "timeline.tsv"
            tl.write_text(timeline_tsv())
            guest = Path(tmp) / "life"  # fresh copy of the embedded guest every run
            await run_step(
                "record", [str(BIN), "life", str(guest), "--record", str(mp4), "--timeline", str(tl)], 900,
            )
        print(f"mp4: {mp4.stat().st_size / 1e6:.2f} MB")
    await make_webp(DOCS / "demo.mp4", DOCS / "demo.webp", a.webp_fps, a.webp_quality)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except StepError as e:
        sys.exit(str(e))
