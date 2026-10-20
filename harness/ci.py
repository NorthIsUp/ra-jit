#!/usr/bin/env python3
"""ci.py: the harnesses CI runs, in parts that take about the same time on a hosted runner.

usage: ci.py PART      run the harnesses of PART for RA_JIT_BACKEND on this machine
       ci.py --parts   the parts this machine has harnesses in, as a JSON list (CI's matrix)
       ci.py --list    which harness runs in which part, on which hosts and backends

Every harness in a part runs even when one before it fails; the part fails at the end and names
them. RA_JIT_BIN is the ra-jit the harnesses run, as for each harness on its own.
"""
import json, os, platform, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
SYSTEM = {"linux": "linux", "darwin": "macos", "win32": "windows"}[sys.platform]
ARCH = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
BACKEND = os.environ.get("RA_JIT_BACKEND") or "cranelift"
KEYS = 3


def on(*hosts, backend=None):
    """Whether a harness runs here: hosts are "linux", "macos", "windows" or "linux-aarch64"..."""
    return lambda: (not hosts or SYSTEM in hosts or f"{SYSTEM}-{ARCH}" in hosts) and backend in (None, BACKEND)


EVERYWHERE = on()
# (part, arguments, environment, where it runs). The order is the order of each part's run.
HARNESSES = [
    ("core", ["difftest.py", "corpus", "--bin", "runner", "--crate", "cases", "--layouts"], {}, EVERYWHERE),
    ("core", ["programs.py"], {}, EVERYWHERE),
    ("core", ["breakers.py"], {}, EVERYWHERE),
    ("core", ["unwinds.py"], {}, EVERYWHERE),
    ("core", ["growth.py"], {}, on(backend="cranelift")),
    ("core", ["splices.py"], {}, EVERYWHERE),
    # A branch reaches 128 MB on AArch64, and the splice has to get past that. Mach-O is not spliced.
    ("core", ["farsplices.py"], {"RA_JIT_SPLICE": "1"}, on("linux-aarch64")),
    ("core", ["asmedits.py"], {}, EVERYWHERE),
    ("core", ["includes.py"], {}, EVERYWHERE),
    ("core", ["worktrees.py"], {}, EVERYWHERE),
    ("core", ["worktrees.py", "--join", "--together"], {}, on("macos")),
    ("core", ["worktrees.py", "--overlay"], {}, on("macos")),
    # Tiers take live edits, which are macOS's alone; the harness picks its backends.
    ("core", ["tiers.py"], {}, on("macos", backend="cranelift")),
    # Generated programs on fixed seeds, and generated edits under a daemon against cold builds.
    # Only x86-64 Linux has been fuzzed so far.
    ("core", ["fuzz/fuzz.py", "smoke", "--backends", BACKEND], {}, on("linux-x86_64")),
    ("core", ["fuzz/edits.py", "smoke", "--backends", BACKEND], {}, on("linux-x86_64")),
    # How much of the corpus LLVM compiled itself, and what it left to Cranelift and why.
    ("core", ["backends.py", "corpus", "runner", "cases", "--bin", "runner", "--levels", "2f"], {}, on(backend="llvm")),
    # DWARF read by gdb out of an ELF executable, or a PDB read by cdb.
    ("core", ["debuginfo.py"], {}, on("linux", "windows")),
    *((f"keys-{k}", ["keys.py", "--shard", f"{k}/{KEYS}"], {}, EVERYWHERE) for k in range(1, KEYS + 1)),
    # Builds for Linux targets besides the host; the cases of another host say so.
    ("buildmodel", ["buildmodel.py", "--backend", BACKEND], {}, on("linux", "windows")),
    ("store-kills", ["storekills.py", "--kills", "100"], {}, EVERYWHERE),
    ("store", ["storecorrupts.py", "--rounds", "4"], {}, EVERYWHERE),
    # It mounts a tmpfs, or fills a disk image on Windows.
    ("store", ["storefull.py"], {}, on("linux", "windows")),
    ("store", ["storeshared.py"], {}, EVERYWHERE),
    ("store", ["storeguards.py"], {}, on("linux")),
    ("daemon", ["stress.py"], {}, EVERYWHERE),
    ("daemon", ["selects.py"], {}, on("linux")),
    # MinIO as one program in ~/s3tools, which CI puts there on Linux.
    ("daemon", ["remote.py"], {}, on("linux")),
]


def parts():
    seen = []
    for part, _, _, here in HARNESSES:
        if here() and part not in seen:
            seen.append(part)
    return seen


def run(part):
    chosen = [(args, env) for p, args, env, here in HARNESSES if p == part and here()]
    if not chosen:
        sys.exit(f"ci.py: no harness of part {part!r} runs on {SYSTEM} {ARCH} with {BACKEND}; the parts here are {parts()}")
    failed, times = [], []
    for args, env in chosen:
        what = " ".join(args)
        print(f"::group::{what}", flush=True)
        start = time.monotonic()
        status = subprocess.run([sys.executable, *args], cwd=HERE, env={**os.environ, **env}).returncode
        times.append((time.monotonic() - start, what))
        print("::endgroup::", flush=True)
        if status:
            failed.append(what)
            print(f"::error::{what} failed (exit status {status})", flush=True)
    for seconds, what in times:
        print(f"{seconds:7.0f} s  {what}")
    print(f"failed: {', '.join(failed) or 'none'}")
    sys.exit(1 if failed else 0)


def main():
    arg = sys.argv[1] if len(sys.argv) == 2 else "--help"
    if arg == "--parts":
        print(json.dumps(parts()))
    elif arg == "--list":
        for part, args, env, _ in HARNESSES:
            print(f"{part:12} {' '.join(f'{k}={v}' for k, v in env.items())} {' '.join(args)}")
    elif arg.startswith("-"):
        print(__doc__)
        sys.exit(0 if arg == "--help" else 2)
    else:
        run(arg)


if __name__ == "__main__":
    main()
