#!/usr/bin/env python3
"""bench.py: what a build costs through `cargo ra-jit`, phase by phase, against plain cargo.

    bench.py [PROJECT...] [--backend cranelift|llvm]... [--runs N] [--out FILE] [--work DIR]
             [--reference DIR] [--keep]
    bench.py --compare BASELINE RESULT [--threshold PERCENT]

Each project is one pinned checkout (cloned into `RA_JIT_PROJECTS` as cargo_ra_jit.py does, and
copied; the clone is never written to). Under each backend, with a store, a daemon and a home
of its own, and plain cargo building the same source into a target directory of its own (under `--reference` when
that is given, which lets the two be on different disks):
  cold       `cargo ra-jit build` from nothing, then the wait until the daemon has the
             workspace loaded and its first build linked
  no-op      `cargo ra-jit build` with nothing changed
  body       one function's body is edited
  signature  a function gets another parameter, and every caller the argument
  leaf       a body in a crate of the workspace that the others depend on
  generic    the body of a generic function that is used at many types
  runtime    (projects with a `runtime` workload) the program ra-jit linked, cargo's debug build
             and cargo's release build run the same command in turn, `--runs` times (at least
             10) each; the output of the three has to be the same
Every edit is made and taken back `--runs` times, or once when the daemon turned it down and
cargo built it; the time of each is the median, and every sample of both sides is kept. None of
them changes what the program does: after the first of each kind the program is run and has
to say what cargo's says. The result also names the commit of ra-jit, the date and the machine
(`RA_JIT_BENCH_MACHINE` names it, for example the instance type); `benchreport.py` turns it
into the README's table and docs/benchmarks.md.

For every phase the result has the wall time of the command, cargo's time for the same
step, the most memory the daemon's workers have held so far, who built it (the daemon, or
cargo and why), whether the daemon added the change to the linked program or linked again
(and why), how many functions it compiled, and what it timed itself: the steps of the build
one after another, what its threads spent on each step of compiling, and why a build started
over when it did.

`--compare` reads two results and exits 1 when a phase of the second is slower or larger
than in the first by more than the threshold, was built by cargo where the daemon built it,
was linked again where it was added, or is missing. Differences under 50 ms are not counted.

The ra-jit of a backend is `RA_JIT_BIN` (cranelift) or `RA_JIT_LLVM_BIN` (llvm).
"""
import glob, json, os, platform, re, shutil, statistics, subprocess, sys, tempfile, time

import cargo_ra_jit
from cargo_ra_jit import HERE, ROOT, TOOLCHAIN, Side, fetch, how_built, program_output, wait_warm

LET = "\n    let _bench_edit = 1u32;"


def body(file, opening):
    """An edit that adds a statement after the line that opens a function."""
    return [(file, opening, opening + LET)]


def parameter(file, name, returns, callers):
    """An edit that gives the method `name` a parameter and every call in `callers` the argument."""
    old = f"fn {name}(&self) -> {returns} {{"
    return [(file, old, old.replace("(&self)", "(&self, _bench_edit: ())")), (callers, f".{name}()", f".{name}(())")]


PROJECTS = {
    "ripgrep": dict(
        repo="BurntSushi/ripgrep", tag="15.1.0", rev="af60c2de9d85e7f3d81c78601669468cf02dabab",
        run=["-n", "--no-heading", "--sort", "path", "fn main", "crates/core/main.rs", "build.rs"],
        edits=dict(
            body=body("crates/core/main.rs", "fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {"),
            signature=parameter("crates/printer/src/standard.rs", "write_line_term", "io::Result<()>", "crates/printer/src/standard.rs"),
            leaf=body("crates/matcher/src/lib.rs", "pub fn len(&self) -> usize {"),
            generic=[("crates/matcher/src/lib.rs", "        let mut last_end = at;", "        let _bench_edit = 1u32;\n        let mut last_end = at;")],
        ),
        # The toolchain's own library source, which is pinned with it.
        runtime=dict(bin="rg", args=["-uuu", "--sort", "path", "-c", r"\w+_unchecked\(|unsafe \{", "{library}"]),
    ),
    "cargo": dict(
        repo="rust-lang/cargo", tag="0.92.0", rev="ea2d97820c16195b0ca3fadb4319fe512c199a43",
        run=["--version"],
        edits=dict(
            body=body("src/cargo/ops/cargo_compile/mod.rs", "pub fn compile<'a>(ws: &Workspace<'a>, options: &CompileOptions) -> CargoResult<Compilation<'a>> {"),
            signature=parameter("src/cargo/util/context/mod.rs", "cli_unstable", "&CliUnstable", "src/**/*.rs"),
            leaf=body("crates/cargo-util/src/paths.rs", "pub fn normalize_path(path: &Path) -> PathBuf {"),
            generic=body("src/cargo/util/context/mod.rs", "pub fn get<'de, T: serde::de::Deserialize<'de>>(&self, key: &str) -> CargoResult<T> {"),
        ),
    ),
    "fd": dict(
        repo="sharkdp/fd", tag="v10.3.0", rev="d38148f0aabdd073b4080cde770f679f3197b920",
        run=["--extension", "rs", "main", "src"],
        edits=dict(
            body=body("src/main.rs", "fn run() -> Result<ExitCode> {"),
            signature=parameter("src/cli.rs", "max_results", "Option<usize>", "src/**/*.rs"),
            leaf=body("src/dir_entry.rs", "pub fn path(&self) -> &Path {"),
        ),
    ),
    "bat": dict(
        repo="sharkdp/bat", tag="v0.26.0", rev="c734087e1d15fb730d7653389c841fd9e3016556",
        run=["--plain", "--color=never", "rustfmt.toml"],
        edits=dict(
            body=body("src/bin/bat/main.rs", "fn run() -> Result<bool> {"),
            signature=parameter("src/assets.rs", "get_theme_set", "&LazyThemeSet", "src/assets.rs"),
            leaf=body("src/assets.rs", "pub fn get_syntax_set(&self) -> Result<&SyntaxSet> {"),
        ),
    ),
    "hyperfine": dict(
        repo="sharkdp/hyperfine", tag="v1.19.0", rev="12fec42098642a19855ead34c8cb1e0be28c8ead",
        run=["--help"],
        edits=dict(
            body=body("src/main.rs", "fn run() -> Result<()> {"),
            signature=parameter("src/command.rs", "get_name", "String", "src/**/*.rs"),
            leaf=body("src/command.rs", "pub fn get_command_line(&self) -> String {"),
        ),
    ),
    "just": dict(
        repo="casey/just", tag="1.43.0", rev="b7e7c46495b34e9407982ffbbb25eb2808ec40b7",
        run=["--list", "--unsorted"],
        edits=dict(
            body=body("src/run.rs", "pub fn run(args: impl Iterator<Item = impl Into<OsString> + Clone>) -> Result<(), i32> {"),
            signature=parameter("src/lexer.rs", "rest", "&'src str", "src/lexer.rs"),
            leaf=body("src/lexer.rs", "  fn lexeme(&self) -> &'src str {"),
        ),
    ),
    # A server: it is built, not run, so nothing checks what it does.
    "axum": dict(
        repo="tokio-rs/axum", tag="axum-v0.8.6", rev="c1bb9c3e90625fb8de4f9c9750f669cb4edac3df",
        build=["-p", "example-hello-world"], run=None,
        edits=dict(
            body=body("examples/hello-world/src/main.rs", "async fn handler() -> Html<&'static str> {"),
            leaf=body("axum-core/src/body.rs", "pub fn empty() -> Self {"),
        ),
    ),
    "nushell": dict(
        repo="nushell/nushell", tag="0.108.0", rev="da141be11b3acaa55b39321e5f9651d02aa2ed6f",
        run=["-n", "-c", "[3 1 2] | sort | str join ','"],
        edits=dict(
            body=body("src/main.rs", "fn main() -> Result<()> {"),
            signature=parameter("crates/nu-protocol/src/value/mod.rs", "coerce_bool", "Result<bool, ShellError>", "crates/**/*.rs"),
            leaf=body("crates/nu-path/src/components.rs", "pub fn components(path: &Path) -> impl Iterator<Item = Component<'_>> {"),
            generic=[("crates/nu-protocol/src/pipeline/pipeline_data.rs", "        F: FnMut(Value) -> Value + 'static + Send,\n    {",
                      "        F: FnMut(Value) -> Value + 'static + Send,\n    {\n        let _bench_edit = 1u32;")],
        ),
    ),
}

BACKENDS = {
    "cranelift": dict(RA_JIT_BACKEND="cranelift"),
    "llvm": dict(RA_JIT_BACKEND="llvm", RA_JIT_OPT="2", RA_JIT_EMIT="fast"),
}
DURATION = re.compile(r"([0-9.]+)(ns|µs|ms|s)\b")
UNITS = {"ns": 1e-9, "µs": 1e-6, "ms": 1e-3, "s": 1.0}
FLOOR_SECS = 0.05


def ra_jit_of(backend):
    if backend == "llvm":
        return os.environ.get("RA_JIT_LLVM_BIN") or os.path.join(ROOT, "target-llvm/release/ra-jit")
    return cargo_ra_jit.RA_JIT


def seconds(text):
    """The seconds a duration as Rust prints it stands for."""
    number, unit = DURATION.search(text).groups()
    return float(number) * UNITS[unit]


def write_edit(ws, changes, back):
    """Makes every change of an edit, or takes it back. The result is how many places changed."""
    places = 0
    for pattern, old, new in changes:
        old, new = (new, old) if back else (old, new)
        for path in sorted(glob.glob(os.path.join(ws, pattern), recursive=True)):
            text = open(path).read()
            if old in text:
                places += text.count(old)
                open(path, "w").write(text.replace(old, new))
    assert places >= len(changes), f"an edit found only {places} places: {changes}"
    return places


class Daemon:
    """The daemon's workers for one checkout: their memory and what they logged."""

    def __init__(self, side):
        self.side, self.read, self.peak = side, {}, 0

    def workers(self):
        _, status = self.side.cargo("status")
        return [(int(pid), log) for pid, log in re.findall(r"worker: pid (\d+), .*, log (.*)$", status.stdout, re.M)]

    def peak_mb(self):
        """The most memory the workers have held at once so far, summed over those there are."""
        total = 0
        for pid, _ in self.workers():
            try:
                status = open(f"/proc/{pid}/status").read()
            except OSError:
                continue
            total += int(re.search(r"VmHWM:\s+(\d+) kB", status)[1])
        self.peak = max(self.peak, round(total / 1024))
        return self.peak

    def said(self, builds=0):
        """What the workers logged since the last call, once `builds` edits are in it."""
        for _ in range(100):
            logs = {log: open(log, "rb").read() for _, log in self.workers() if os.path.exists(log)}
            text = "".join(whole[self.read.get(log, 0):].decode(errors="replace") for log, whole in logs.items())
            if text.count("edit-to-binary:") >= builds:
                break
            time.sleep(0.02)
        self.read.update((log, len(whole)) for log, whole in logs.items())
        return text


def link_kind(text):
    """How the builds in `text` reached the program: added to it, or linked again and why."""
    links = re.findall(r"^link: \S+ \((.*)\)$", text, re.M)
    if not links:
        return None
    again = [l for l in links if not l.startswith("spliced")]
    if not again:
        return "spliced"
    why = re.search(r"not spliced: (.*)", again[0])
    return "full link" + (f": {why[1][:160]}" if why else "")


def daemon_steps(text):
    """The steps the daemon timed in `text`, in milliseconds, summed over its builds."""
    steps = {}
    for line in re.findall(r"^timeline-us: (.*)$", text, re.M):
        for name, micros in (pair.split("=") for pair in line.split()):
            # How long ago the file was saved is not the daemon's time.
            if name != "notice":
                steps[name] = round(steps.get(name, 0) + int(micros) / 1000, 2)
    return steps


def cold_steps(text):
    """The steps of loading a workspace and building it for the first time, in seconds."""
    steps = {}
    for name, pattern in (("load", r"^load workspace: (\S+)"), ("resolve", r"^resolve: \d+ crates in (\S+)"),
                          ("compile", r"^cold: compiled .* in (\S+) \("), ("emit", r"^cold: compiled .*\(\+(\S+) emit\)"),
                          ("link", r"^link: (\S+) \(")):
        found = re.findall(pattern, text, re.M)
        if found:
            steps[name] = round(sum(seconds(f) for f in found), 3)
    return steps


def thread_steps(text):
    """What the daemon's threads spent on each step of compiling a function and on each of
    the producer's, in seconds summed over the threads and the builds in `text`."""
    steps = {}
    for line in re.findall(r"^steps: (.*)$", text, re.M):
        for name, took in re.findall(r"([a-z ]+) ([0-9.]+(?:ns|µs|ms|s))(?:,|$)", line):
            steps[name.strip()] = round(steps.get(name.strip(), 0) + seconds(took), 3)
    for producer, took in re.findall(r"^backend: (\w+) compiled \d+ functions, \d+ bytes, (\S+) ", text, re.M):
        steps[producer] = round(steps.get(producer, 0) + seconds(took), 3)
    return steps


def rounds(text):
    """Why the builds in `text` started over, each time they did."""
    return re.findall(r"^round \d+: \S+, started over: (.*)$", text, re.M)


def compiled(text):
    """How many functions the builds in `text` compiled."""
    return sum(int(n) for n in re.findall(r"^stats: (\d+) of \d+ functions compiled", text, re.M))


def measure(name, spec, backend, runs, work, reference_dir):
    """Runs every phase of one project under one backend. The result is the phases by name."""
    ra_jit = ra_jit_of(backend)
    ws = os.path.realpath(os.path.join(work, f"{name}-{backend}"))
    shutil.copytree(fetch(name, spec), ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", ".git"))
    env = cargo_ra_jit.base_env(os.path.join(work, f"home-{name}-{backend}"))
    env["PATH"] = os.path.dirname(ra_jit) + os.pathsep + env["PATH"]
    env.update(BACKENDS[backend], RA_JIT_STATS="1", RA_JIT_CACHE=os.path.join(work, f"store-{name}-{backend}"))
    # Neither side gets a compiler cache: a cold build is a cold build.
    env.pop("RUSTC_WRAPPER", None)
    ours, theirs = Side(ws, env, True), Side(ws, env, False)
    if reference_dir:
        theirs.env["CARGO_TARGET_DIR"] = os.path.join(reference_dir, f"target-ref-{name}-{backend}")
    daemon, phases, wrong = Daemon(ours), {}, []
    build, run = spec.get("build", []), spec.get("run")
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    ours.cargo("stop")

    def both():
        (took, mine), (reference, other) = ours.cargo("build", *build), theirs.cargo("build", *build)
        for side, ran in (("ra-jit", mine), ("cargo", other)):
            if ran.returncode != 0:
                raise SystemExit(f"{name} {backend}: the {side} build failed:\n{ran.stderr[-3000:]}")
        built = how_built(mine.stderr)
        if built != "daemon":
            # A build cargo did has the daemon load again; the next one is not timed against that.
            wait_warm(ours, ws)
        return took, reference, built

    def runs_right(label):
        if run is None:
            return
        _, mine = ours.cargo("run", *build, "--", *run)
        if program_output(mine) != expected:
            wrong.append(f"{label}: the program says {program_output(mine)!r}")

    def report(label, phase):
        phases[label] = phase
        print(f"  {label:10} ra-jit {phase['wall_s']:8.3f}s  cargo {phase['cargo_s']:8.3f}s  rss {phase['rss_mb']:6} MB  "
              f"{phase['built'][:60]}  {phase.get('link') or ''}", flush=True)

    # Cargo's own cold build waits until the daemon is warm: the two would share the cores.
    took, mine = ours.cargo("build", *build)
    warm, states = wait_warm(ours, ws)
    if mine.returncode != 0 or states != ["ready"]:
        raise SystemExit(f"{name} {backend}: the daemon did not get ready: {states}\n{mine.stderr[-3000:]}")
    said, built = daemon.said(), how_built(mine.stderr)
    reference, other = theirs.cargo("build", *build)
    if other.returncode != 0:
        raise SystemExit(f"{name} {backend}: the cargo build failed:\n{other.stderr[-3000:]}")
    expected = program_output(theirs.cargo("run", *build, "--", *run)[1]) if run is not None else None
    report("cold", dict(wall_s=round(took + warm, 3), cargo_s=round(reference, 3), rss_mb=daemon.peak_mb(), built=built,
                        build_s=round(took, 3), warm_s=round(warm, 3), compiled=compiled(said), steps_s=cold_steps(said),
                        threads_s=thread_steps(said), started_over=rounds(said)))

    samples = [both() for _ in range(max(runs, 3))]
    said = daemon.said()
    report("no-op", dict(wall_s=round(statistics.median(s[0] for s in samples), 3), cargo_s=round(statistics.median(s[1] for s in samples), 3),
                         rss_mb=daemon.peak_mb(), built=samples[-1][2], compiled=compiled(said),
                         samples_s=[round(s[0], 3) for s in samples], cargo_samples_s=[round(s[1], 3) for s in samples]))
    runs_right("no-op")

    for kind, changes in spec["edits"].items():
        made, back, first = [], [], None
        for turn in range(runs):
            places = write_edit(ws, changes, False)
            took, reference, built = both()
            said = daemon.said(1 if built == "daemon" else 0)
            made.append(dict(wall_s=took, cargo_s=reference, built=built, link=link_kind(said), compiled=compiled(said), steps_ms=daemon_steps(said),
                             threads_s=thread_steps(said), started_over=rounds(said)))
            if turn == 0:
                runs_right(kind)
                daemon.said()
            write_edit(ws, changes, True)
            took, reference, built = both()
            said = daemon.said(1 if built == "daemon" else 0)
            back.append(dict(wall_s=took, cargo_s=reference, built=built, link=link_kind(said), compiled=compiled(said)))
            # An edit the daemon turned down is one finding, not a time to take a median of,
            # and each has it load the workspace again.
            if made[-1]["built"] != "daemon":
                break
        # The slowest of them is the one to look at; the median is the one to compare.
        slowest = max(made, key=lambda s: s["wall_s"])
        median = lambda rows, key: round(statistics.median(r[key] for r in rows), 3)
        report(kind, dict(wall_s=median(made, "wall_s"), cargo_s=median(made, "cargo_s"), rss_mb=daemon.peak_mb(),
                          built=slowest["built"], link=slowest["link"], compiled=max(s["compiled"] for s in made), places=places,
                          steps_ms=slowest["steps_ms"], threads_s=slowest["threads_s"], started_over=slowest["started_over"], slowest_s=round(slowest["wall_s"], 3),
                          back_s=median(back, "wall_s"), back_cargo_s=median(back, "cargo_s"),
                          back_link=max(back, key=lambda s: s["wall_s"])["link"],
                          samples_s=[round(s["wall_s"], 3) for s in made], cargo_samples_s=[round(s["cargo_s"], 3) for s in made]))
    ours.cargo("stop")
    result = dict(phases=phases, wrong=wrong, tag=spec["tag"], rev=spec["rev"])
    if "runtime" in spec:
        result["runtime"] = runtime(name, spec, theirs, ws, build, max(runs, 10), wrong)
    return result


def library():
    """The source of the toolchain's standard library, which is pinned with the toolchain."""
    sysroot = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True,
                             env=dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN), check=True).stdout.strip()
    return os.path.join(sysroot, "lib/rustlib/src/rust/library")


def runtime(name, spec, theirs, ws, build, runs, wrong):
    """How fast the program ra-jit linked runs, against cargo's debug and release builds of the
    same source. The three are run in turn, `runs` times each after two runs to warm the page
    cache, so that the machine's drift falls on all of them alike. Seconds per run."""
    spec = spec["runtime"]
    _, released = theirs.cargo("build", "--release", *build)
    if released.returncode != 0:
        raise SystemExit(f"{name}: the cargo release build failed:\n{released.stderr[-3000:]}")
    target = theirs.env["CARGO_TARGET_DIR"]
    programs = {"ra-jit": os.path.join(ws, "target/debug", spec["bin"]), "cargo debug": os.path.join(target, "debug", spec["bin"]),
                "cargo release": os.path.join(target, "release", spec["bin"])}
    args = [a.replace("{library}", library()) for a in spec["args"]]
    times, outputs = {label: [] for label in programs}, {}
    for turn in range(runs + 2):
        for label, program in programs.items():
            began = time.perf_counter()
            ran = subprocess.run([program, *args], cwd=ws, capture_output=True)
            took = time.perf_counter() - began
            outputs.setdefault(label, (ran.returncode, ran.stdout))
            if turn >= 2:
                times[label].append(took)
    for label, said in outputs.items():
        if said != outputs["cargo debug"]:
            wrong.append(f"runtime: the program ra-jit linked ({label}) says something else than cargo's")
    result = {label: dict(median_s=round(statistics.median(t), 4), min_s=round(min(t), 4), max_s=round(max(t), 4), runs=len(t))
              for label, t in times.items()}
    print("  runtime    " + "  ".join(f"{label} {r['median_s']:.3f}s" for label, r in result.items()), flush=True)
    return dict(command=[spec["bin"], *spec["args"]], **result)


def machine():
    """What the numbers were measured on."""
    def first_line(*argv):
        try:
            ran = subprocess.run(argv, capture_output=True, text=True, env=dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN))
            return (ran.stdout or ran.stderr).strip().splitlines()[0]
        except (OSError, IndexError):
            return None

    def read(path, pattern):
        try:
            found = re.search(pattern, open(path).read(), re.M)
            return found[1].strip() if found else None
        except OSError:
            return None

    memory = read("/proc/meminfo", r"^MemTotal:\s+(\d+) kB")
    # A run pinned to some of the cores (taskset) has those, and cargo's jobs follow them.
    cores = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count()
    return dict(
        # Named by whoever runs it (the workflow names the instance type).
        name=os.environ.get("RA_JIT_BENCH_MACHINE"),
        cpu=read("/proc/cpuinfo", r"^model name\s*:\s*(.*)$"), cores=cores, machine_cores=os.cpu_count(),
        memory_gib=round(int(memory) / 1024 / 1024) if memory else None,
        os=platform.system(), arch=platform.machine(), kernel=platform.release(),
        rustc=first_line("rustc", "-V"), cc=first_line("cc", "--version"), ld=first_line("ld", "--version"),
        llvm=first_line("llvm-config", "--version"),
        incremental=os.environ.get("CARGO_INCREMENTAL", "profile default (on for dev)"),
        wrapper=None, linker="the toolchain's default (cc)",
    )


def ra_jit_commit():
    """The commit of ra-jit measured: `RA_JIT_BENCH_COMMIT` when the caller knows better than the
    checkout (a branch that changes only the harness, measured for main)."""
    if os.environ.get("RA_JIT_BENCH_COMMIT"):
        return os.environ["RA_JIT_BENCH_COMMIT"]
    ran = subprocess.run(["git", "-C", ROOT, "rev-parse", "HEAD"], capture_output=True, text=True)
    return ran.stdout.strip() or None


def compare(baseline, result, threshold):
    """What in `result` is worse than in `baseline` by more than `threshold` percent."""
    worse = []
    for key, was in baseline["results"].items():
        now = result["results"].get(key)
        if now is None:
            worse.append(f"{key}: not measured")
            continue
        worse += [f"{key}: {w}" for w in now.get("wrong", [])]
        for label, old in was["phases"].items():
            new = now["phases"].get(label)
            if new is None:
                worse.append(f"{key} {label}: not measured")
                continue
            for field, floor, unit in (("wall_s", FLOOR_SECS, "s"), ("rss_mb", 50, " MB"), ("compiled", 10, " functions")):
                before, after = old.get(field), new.get(field)
                if before is not None and after is not None and after > before * (1 + threshold / 100) and after - before > floor:
                    worse.append(f"{key} {label}: {field} {before}{unit} -> {after}{unit} (+{100 * (after - before) / max(before, 1e-9):.0f}%)")
            if old["built"] == "daemon" and new["built"] != "daemon":
                worse.append(f"{key} {label}: built by {new['built']}, was the daemon")
            if old.get("link") == "spliced" and new.get("link") != "spliced":
                worse.append(f"{key} {label}: {new.get('link')}, was spliced")
    return worse


def options(argv, name, default=None):
    """The values given to every `--name VALUE` in `argv`, which loses them."""
    found = []
    while name in argv:
        at = argv.index(name)
        found.append(argv[at + 1])
        del argv[at:at + 2]
    return found or ([default] if default is not None else [])


def main():
    argv = sys.argv[1:]
    threshold = float(options(argv, "--threshold", "25")[0])
    if "--compare" in argv:
        argv.remove("--compare")
        baseline, result = (json.load(open(path)) for path in argv)
        worse = compare(baseline, result, threshold)
        print("\n".join(worse) or f"nothing over {threshold:.0f}% worse than the baseline")
        sys.exit(1 if worse else 0)
    backends, runs = options(argv, "--backend") or list(BACKENDS), int(options(argv, "--runs", "3")[0])
    out = options(argv, "--out", os.path.join(HERE, "bench.json"))[0]
    work = (options(argv, "--work") or [None])[0] or tempfile.mkdtemp(prefix="ra-jit-bench-")
    reference_dir = (options(argv, "--reference") or [None])[0]
    keep = "--keep" in argv
    names = [a for a in argv if not a.startswith("--")] or list(PROJECTS)
    os.makedirs(work, exist_ok=True)
    result = dict(toolchain=TOOLCHAIN, machine=platform.machine(), cores=os.cpu_count(), runs=runs,
                  commit=ra_jit_commit(), date=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), host=machine(),
                  backends={b: BACKENDS[b] for b in backends}, results={})
    for name in names:
        for backend in backends:
            print(f"{name} {backend} ({ra_jit_of(backend)}; work {work})", flush=True)
            result["results"][f"{name}/{backend}"] = measured = measure(name, PROJECTS[name], backend, runs, work, reference_dir)
            for line in measured["wrong"]:
                print(f"  WRONG {line}", flush=True)
            json.dump(result, open(out, "w"), indent=1)
            if not keep:
                for at in filter(None, (work, reference_dir)):
                    for entry in os.listdir(at):
                        if entry.endswith(f"{name}-{backend}"):
                            shutil.rmtree(os.path.join(at, entry), ignore_errors=True)
    wrong = [f"{key}: {line}" for key, measured in result["results"].items() for line in measured["wrong"]]
    print(f"wrote {out}; wrong: {len(wrong)}")
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
