#!/usr/bin/env python3
"""Build the ra-jit that a release ships, optimized with a profile of its own work (PGO).

  build_release.py --target x86_64-unknown-linux-gnu

Leaves ra-jit, cargo-ra-jit and ra-jit-rustc in target/<target>/dist, as
`cargo build -p ra-jit --profile dist --locked --target <target>` does, in four steps:

  1. that build instrumented (`-Cprofile-generate`), into target/pgo-instrumented;
  2. that ra-jit run on edits of projects the README's benchmarks do not measure (fd, bat,
     just and hyperfine, at the commits harness/bench.py pins: a cold start and every kind of
     edit, twice, through harness/abbench.py --train);
  3. what it counted merged with the toolchain's llvm-profdata (`rustup component add
     llvm-tools`), refused when it is too small to be the workers' (see below);
  4. the build again with `-Cprofile-use`.

Against `cargo build --release` on Linux (docs/reports/RELEASE-BUILD.md): the cold start 14%
shorter on nushell and ripgrep, an edit of a function 8% shorter on nushell, the same memory.
It takes about four times as long.

A killed process writes no counters: `cargo ra-jit stop` kills the workers, so abbench ends a
training session by ending the daemon, after which each worker sees its input close and exits
by itself. The `cc` crate passes `-Cprofile-generate`/`-Cprofile-use` on to the C compiler;
on macOS Apple clang's profile format is not rustc's LLVM's and the instrumented ra-jit
crashes at start (rust-lang/rust#163640), so the C code (mimalloc, ring) is built without
them. macOS and Linux; stdlib only.
"""

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HARNESS = ROOT / "harness"
TOOLCHAIN = (HARNESS / "toolchain").read_text().strip()
# A training run that reached the workers counts tens of billions; one that lost them a few
# million (the client's own short runs).
MIN_COUNT = 1_000_000_000


def run(argv, **kwargs):
    print("+", " ".join(map(str, argv)), flush=True)
    subprocess.run(argv, check=True, **kwargs)


def build(target, target_dir, rustflags, cflags):
    """`cargo build -p ra-jit --profile dist` for `target` with `rustflags`, and `cflags` for
    the C code that crates build with `cc`."""
    triple = target.replace("-", "_")
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, CARGO_TARGET_DIR=str(target_dir), RUSTFLAGS=rustflags)
    for name in ("CFLAGS", "CXXFLAGS"):
        env[f"{name}_{triple}"] = " ".join(filter(None, [os.environ.get(f"{name}_{triple}"), cflags]))
    run(["cargo", "build", "-p", "ra-jit", "--profile", "dist", "--locked", "--target", target], cwd=ROOT, env=env)
    return target_dir / target / "dist"


def llvm_tool(name):
    libdir = subprocess.run(["rustc", "--print", "target-libdir"], capture_output=True, text=True, check=True,
                            env=dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN)).stdout.strip()
    tool = Path(libdir).parent / "bin" / name
    if not tool.exists():
        sys.exit(f"error: build_release: {tool} is missing: `rustup component add llvm-tools --toolchain {TOOLCHAIN}`")
    return tool


def train(binaries, work, raw):
    """Runs the instrumented ra-jit in `binaries` on the training edits, each of its processes
    writing its counters to a file of its own in `raw` as it exits."""
    env = dict(os.environ, LLVM_PROFILE_FILE=str(raw / "%p.profraw"))
    env.pop("RUST_BACKTRACE", None)
    run([sys.executable, "abbench.py", "--bin", f"train={binaries}", "fd", "bat", "just", "hyperfine",
         "--rounds", "1", "--runs", "2", "--kinds", "body,leaf,signature,generic,no-op", "--train",
         "--work", str(work / "abbench"), "--out", str(work / "train.json")], cwd=HARNESS, env=env)
    # A process still exiting is still writing its file.
    began = time.time()
    while subprocess.run(["pgrep", "-f", str(binaries)], capture_output=True).returncode == 0:
        if time.time() - began > 300:
            sys.exit(f"error: build_release: programs of {binaries} still run 5 minutes after the training")
        time.sleep(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--target", required=True)
    args = parser.parse_args()
    profdata = llvm_tool("llvm-profdata")
    # Short: the daemon's sockets live under it, and a Unix socket's path has a limit.
    work = Path(tempfile.mkdtemp(prefix="rjpgo-", dir="/tmp"))
    raw = work / "raw"
    raw.mkdir()

    instrumented = build(args.target, ROOT / "target" / "pgo-instrumented", f"-Cprofile-generate={raw}", "-fno-profile-generate")
    train(instrumented, work, raw)

    merged = work / "ra-jit.profdata"
    run([profdata, "merge", "-o", merged, *sorted(raw.glob("*.profraw"))])
    shown = subprocess.run([profdata, "show", merged], capture_output=True, text=True, check=True).stdout
    total = int(re.search(r"^Total count: (\d+)", shown, re.M)[1])
    print(f"build_release: the profile counts {total:,}", flush=True)
    if total < MIN_COUNT:
        sys.exit(f"error: build_release: the profile counts {total:,}, fewer than {MIN_COUNT:,}: the workers' counters were lost")

    build(args.target, ROOT / "target", f"-Cprofile-use={merged}", "-fno-profile-use")
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
