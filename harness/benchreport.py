#!/usr/bin/env python3
"""benchreport.py: the README's build table and docs/benchmarks.md, from a result of bench.py.

    benchreport.py RESULT [--stored FILE] [--noise PERCENT] [--max-age HOURS] [--force] [--check]

RESULT is what `bench.py --out` wrote. It is compared with the stored result (`--stored`,
default docs/benchmarks/linux-x86_64.json). When the two differ by more than noise, the stored
one is replaced and the parts of README.md and docs/benchmarks.md between
`<!-- bench:start -->` and `<!-- bench:end -->` are written again from it; otherwise nothing is
written. More than noise is any of:
  - a median time of either side, in any phase, moved by more than `--noise` percent (default
    10) and by more than 50 ms
  - a phase the daemon built is now built by cargo, or the other way round
  - a project or backend is new or gone
  - the stored result is older than `--max-age` hours (default 24), so that the commit the
    README names is never more than a day behind main
  - `--force`
A result of `bench.py --runtime` (harness/runbench.py's numbers) writes the README's second
table instead, between `<!-- runbench:start -->` and `<!-- runbench:end -->`: how fast each
benchmark's program runs built by ra-jit's LLVM against `cargo build --release`, and the
geometric mean. It is stored in docs/benchmarks/runtime-<os>-<arch>.json (`--stored` to say
otherwise), and differs when a median of any build moved by more than noise, a benchmark is new
or gone, or the stored run is older than `--max-age`. A build that failed, or whose program did
not do what the release build's did, has the whole result refused.
A result in which a program said something else than cargo's (`wrong`) is refused: exit 2,
nothing written. `--check` writes nothing and exits 1 when it would have written.

The last line says `changed` or `unchanged`, and so does `changed=true|false` in
$GITHUB_OUTPUT when that is set.
"""
import datetime, json, math, os, re, shutil, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
STORED = os.path.join(ROOT, "docs/benchmarks/linux-x86_64.json")
README = os.path.join(ROOT, "README.md")
DOC = os.path.join(ROOT, "docs/benchmarks.md")
START, END = "<!-- bench:start -->", "<!-- bench:end -->"
RUN_START, RUN_END = "<!-- runbench:start -->", "<!-- runbench:end -->"
REPO = "https://github.com/indexable-inc/ra-jit"
FLOOR_S = 0.05

# The order of the projects and phases in the tables, and what the README calls each edit.
PROJECTS = ["ripgrep", "fd", "bat", "hyperfine", "just", "axum", "nushell", "cargo"]
EDITS = {
    "body": "Edit a function",
    "leaf": "Edit a widely used function",
    "signature": "Change a signature",
    "generic": "Edit a generic function",
    "no-op": "Nothing changed",
}
README_PHASES = ["body", "leaf", "signature", "no-op"]
BACKEND_NAMES = {"cranelift": "Cranelift", "llvm": "LLVM"}


def seconds(value):
    if value is None:
        return "-"
    return f"{value * 1000:.0f} ms" if value < 1 else f"{value:.2f} s"


def speedup(cargo, ours):
    if not cargo or not ours:
        return ""
    ratio = cargo / ours
    return f"{ratio:.0f}x" if ratio >= 10 else f"{ratio:.1f}x"


def spread(samples):
    if not samples:
        return ""
    return f"{seconds(min(samples))} to {seconds(max(samples))}"


def ordered(result, backend):
    keys = [k for k in result["results"] if k.endswith("/" + backend)]
    rank = lambda k: (PROJECTS.index(k.split("/")[0]) if k.split("/")[0] in PROJECTS else len(PROJECTS), k)
    return sorted(keys, key=rank)


def project_name(key, measured):
    tag = measured.get("tag", "")
    return f"{key.split('/')[0]} {tag.lstrip('v').removeprefix('axum-v')}".strip()


def cargo_built(phase):
    return phase is not None and phase.get("built") != "daemon"


def short(commit):
    return (commit or "unknown")[:10]


def machine_line(result):
    host = result.get("host") or {}
    cores, of = host.get("cores") or result.get("cores"), host.get("machine_cores")
    cpu = re.sub(r"( \d+-Core)? Processor$", "", host.get("cpu") or "") or None
    details = [p for p in (cpu, f"{cores} of its {of} cores" if of and of != cores else f"{cores} cores",
                           f"{host['memory_gib']} GiB" if host.get("memory_gib") else None) if p]
    return f"{host.get('name') or 'one machine'} ({', '.join(details)})"


def provenance(result, which=True):
    """Who ran it: the CI run, or a person with the same harness; with `which`, which build of
    ra-jit it timed."""
    built = (f" The ra-jit measured is `cargo build --release`; the binaries a release ships are the `dist` "
             f"profile with PGO, which start and edit faster ([how]({REPO}/blob/main/docs/reports/RELEASE-BUILD.md)).") if which else ""
    if result.get("run_url"):
        return f"Measured by [CI]({result['run_url']}) on the dedicated benchmark machine, which re-runs it after pushes to main." + built
    return "Measured by hand with `harness/bench.py`; the `bench-readme` workflow re-runs it on the dedicated benchmark machine." + built


def readme_section(result):
    backend = "cranelift"
    keys = ordered(result, backend)
    commit = result.get("commit")
    lines = [
        START,
        f"Edit, then build, on real projects at pinned commits. Each cell is incremental `cargo build` → "
        f"`cargo ra-jit build`, both warm, same linker, median of {result['runs']} edits. Linux x86-64, "
        f"{machine_line(result)}, ra-jit [`{short(commit)}`]({REPO}/commit/{commit}) on {result['date'][:10]}.",
        "",
        "| Project | " + " | ".join(EDITS[p] for p in README_PHASES) + " |",
        "|---|" + "---:|" * len(README_PHASES),
    ]
    fell_back = False
    for key in keys:
        measured = result["results"][key]
        cells = []
        for label in README_PHASES:
            phase = measured["phases"].get(label)
            if phase is None:
                cells.append("")
                continue
            mark = ""
            if cargo_built(phase):
                mark, fell_back = " (c)", True
            cells.append(f"{seconds(phase['cargo_s'])} → **{seconds(phase['wall_s'])}**{mark} ({speedup(phase['cargo_s'], phase['wall_s'])})")
        lines.append(f"| {project_name(key, measured)} | " + " | ".join(cells) + " |")
    lines.append("")
    if fell_back:
        lines.append("(c): the daemon handed this edit to cargo (a fallback, so about cargo's time).")
        lines.append("")
    lines.append(provenance(result) + " Cold builds, the spread of every number, the LLVM tier and how fast the "
                 "built programs run: [docs/benchmarks.md](docs/benchmarks.md) ([raw JSON](docs/benchmarks/linux-x86_64.json)).")
    lines.append(END)
    return "\n".join(lines)


def doc_section(result):
    commit = result.get("commit")
    host = result.get("host") or {}
    lines = [START, f"### Latest run", "",
             f"ra-jit [`{short(commit)}`]({REPO}/commit/{commit}), {result['date']}, Rust {result['toolchain']}, "
             f"{result['runs']} runs of each edit. Raw: [benchmarks/linux-x86_64.json](benchmarks/linux-x86_64.json). "
             f"{provenance(result)}", "",
             "| Machine | |", "|---|---|"]
    for label, field in (("Name", "name"), ("CPU", "cpu"), ("Cores", "cores"), ("Memory (GiB)", "memory_gib"), ("Kernel", "kernel"),
                         ("rustc", "rustc"), ("C compiler", "cc"), ("Linker", "ld"), ("LLVM", "llvm"),
                         ("Incremental", "incremental"), ("Compiler cache", "wrapper")):
        value = host.get(field)
        lines.append(f"| {label} | {value if value is not None else 'none' if field == 'wrapper' else '-'} |")
    lines.append("")
    for backend in [b for b in BACKEND_NAMES if any(k.endswith("/" + b) for k in result["results"])]:
        keys = ordered(result, backend)
        lines += [f"#### Edits, {BACKEND_NAMES[backend]}", "",
                  "Median, then the fastest and slowest run. Built: who built the edit (daemon, or cargo and why). "
                  "Link: the change was spliced into the program, or it was linked again.", "",
                  "| Project | Edit | cargo | ra-jit | | cargo runs | ra-jit runs | Built | Link |",
                  "|---|---|---:|---:|---:|---|---|---|---|"]
        for key in keys:
            measured = result["results"][key]
            for label in ("body", "leaf", "signature", "generic", "no-op"):
                phase = measured["phases"].get(label)
                if phase is None:
                    continue
                lines.append(f"| {project_name(key, measured)} | {EDITS[label]} | {seconds(phase['cargo_s'])} | {seconds(phase['wall_s'])} | "
                             f"{speedup(phase['cargo_s'], phase['wall_s'])} | {spread(phase.get('cargo_samples_s'))} | {spread(phase.get('samples_s'))} | "
                             f"{phase['built'][:80]} | {phase.get('link') or ''} |")
        lines += ["", f"#### Cold, {BACKEND_NAMES[backend]}", "",
                  "From an empty target directory and an empty store, crates already downloaded. One run.", "",
                  "| Project | `cargo build` | `cargo ra-jit build` | Daemon ready after | Functions compiled | Daemon memory |",
                  "|---|---:|---:|---:|---:|---:|"]
        for key in keys:
            measured = result["results"][key]
            cold = measured["phases"].get("cold")
            if cold:
                lines.append(f"| {project_name(key, measured)} | {seconds(cold['cargo_s'])} | {seconds(cold.get('build_s'))} | "
                             f"{seconds(cold.get('warm_s'))} | {cold.get('compiled', 0):,} | {cold.get('rss_mb', 0):,} MB |")
        timed = [k for k in keys if result["results"][k].get("runtime")]
        if timed:
            lines += ["", f"#### How fast the program runs, {BACKEND_NAMES[backend]}", "",
                      "The same command run by the program ra-jit linked and by cargo's debug and release builds, in turn. "
                      "Median, then fastest and slowest.", "",
                      "| Project | Command | ra-jit | cargo debug | cargo release |", "|---|---|---:|---:|---:|"]
            for key in timed:
                run = result["results"][key]["runtime"]
                cell = lambda r: f"{seconds(r['median_s'])} ({seconds(r['min_s'])} to {seconds(r['max_s'])})"
                command = " ".join(run["command"]).replace("{library}", "<the toolchain's library source>").replace("|", "\\|")
                lines.append(f"| {project_name(key, result['results'][key])} | `{command}` | {cell(run['ra-jit'])} | "
                             f"{cell(run['cargo debug'])} | {cell(run['cargo release'])} |")
        lines.append("")
    lines.append(END)
    return "\n".join(lines)


def platform(result):
    host = result.get("host") or {}
    system = {"Linux": "Linux", "Darwin": "macOS"}.get(host.get("os"), host.get("os") or "unknown OS")
    arch = {"x86_64": "x86-64", "AMD64": "x86-64", "arm64": "arm64", "aarch64": "arm64"}.get(host.get("arch"), host.get("arch") or "")
    return f"{system} {arch}".strip()


def runtime_wrong(result):
    """Why a runtime result may not be published: a build that failed or whose program
    behaved otherwise than cargo's release build."""
    wrong = []
    for name, program in (result.get("runbench") or {}).get("programs", {}).items():
        for build, info in program["builds"].items():
            if info.get("error"):
                wrong.append(f"{name} {build}: the build failed")
            for bench in info.get("differs", []):
                wrong.append(f"{bench} {build}: the program did not do what cargo's release build did")
        for bench, timed in program.get("benches", {}).items():
            for build in ("release", "ra-jit"):
                if build not in timed:
                    wrong.append(f"{bench} {build}: not timed")
    return wrong


def times(ratio):
    return f"{ratio:.2f}x" if ratio < 10 else f"{ratio:.0f}x"


def runtime_section(result):
    """The README's table of how fast what ra-jit's LLVM builds runs, against cargo's release build."""
    measured, commit = result["runbench"], result.get("commit")
    builds = measured["builds"]
    checked = "checked" in builds
    rows, rounds = [], None
    for program in measured["programs"].values():
        for bench, row in program["benches"].items():
            rounds = len(row["release"]["runs"])
            rows.append(f"| {bench} | {seconds(row['release']['median_s'])} | **{seconds(row['ra-jit']['median_s'])}** | "
                        f"{times(row['ra-jit']['ratio'])} |" + (f" {times(row['checked']['ratio'])} |" if checked else ""))
    geomean = measured.get("geomean") or {}
    lines = [
        RUN_START,
        f"How fast the programs run. Each program is built by `cargo build --release` and by ra-jit with LLVM at "
        f"`-O3`, every crate of it compiled by ra-jit, with the release profile; each benchmark is then run "
        f"{'once' if rounds == 1 else f'{rounds} times, the builds taking turns, median'}. A run is published only "
        f"when every program did what the release build's did. {platform(result)}, {machine_line(result)}, ra-jit "
        f"[`{short(commit)}`]({REPO}/commit/{commit}) on {result['date'][:10]}.",
        "",
        "| Benchmark | `cargo build --release` | ra-jit, LLVM | ra-jit ÷ release |" + (" release with dev checks ÷ release |" if checked else ""),
        "|---|---:|---:|---:|" + ("---:|" if checked else ""),
        *rows,
    ]
    if geomean.get("ra-jit"):
        lines.append(f"| **Geometric mean** | | | **{times(geomean['ra-jit'])}** |" +
                     (f" {times(geomean['checked'])} |" if checked and geomean.get("checked") else (" |" if checked else "")))
    lines += ["", "Release with dev checks: `cargo build --release` with `-C overflow-checks=on -C debug-assertions=on`. "
              + (provenance(result, which=False).replace("`harness/bench.py`", "`harness/bench.py --runtime`") if result.get("run_url") else
                 "Measured by hand with `harness/bench.py --runtime` (harness/runbench.py); the `bench-readme` workflow "
                 "re-runs it on the dedicated benchmark machine."), RUN_END]
    return "\n".join(lines)


def runtime_differences(stored, result, noise, max_age_h):
    if stored is None or not stored.get("runbench"):
        return ["nothing stored"]
    why = []
    when = lambda r: datetime.datetime.strptime(r["date"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
    if (when(result) - when(stored)).total_seconds() / 3600 > max_age_h:
        why.append("the stored result is old")
    rows = lambda r: {(p, b): row for p, prog in r["runbench"]["programs"].items() for b, row in prog["benches"].items()}
    was, now = rows(stored), rows(result)
    if set(was) != set(now):
        why.append(f"benchmarks: {sorted(was)} -> {sorted(now)}")
    for key, row in now.items():
        for build, cell in row.items():
            old = was.get(key, {}).get(build)
            if old and abs(cell["median_s"] - old["median_s"]) / old["median_s"] * 100 > noise:
                why.append(f"{key[1]} {build}: {old['median_s']} -> {cell['median_s']}")
    return why


def splice(path, section, start=START, end=END):
    text = open(path).read()
    pattern = re.compile(re.escape(start) + ".*?" + re.escape(end), re.S)
    if not pattern.search(text):
        raise SystemExit(f"{path} has no {start} ... {end} section")
    open(path, "w").write(pattern.sub(lambda _: section, text, count=1))


def differences(stored, result, noise, max_age_h):
    """Why `result` is worth writing over `stored`; empty when it is within noise."""
    if stored is None:
        return ["nothing stored"]
    why = []
    if set(stored["results"]) != set(result["results"]):
        why.append(f"projects: {sorted(stored['results'])} -> {sorted(result['results'])}")
    when = lambda r: datetime.datetime.strptime(r["date"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)
    age = (when(result) - when(stored)).total_seconds() / 3600
    if age > max_age_h:
        why.append(f"the stored result is {age:.0f} h old")
    for key, now in result["results"].items():
        was = stored["results"].get(key)
        if was is None:
            continue
        for label, phase in now["phases"].items():
            old = was["phases"].get(label)
            if old is None:
                why.append(f"{key} {label}: new")
                continue
            for field in ("wall_s", "cargo_s"):
                before, after = old.get(field), phase.get(field)
                if before and after and abs(after - before) > FLOOR_S and abs(after - before) / before * 100 > noise:
                    why.append(f"{key} {label} {field}: {before} -> {after}")
            if (old.get("built") == "daemon") != (phase.get("built") == "daemon"):
                why.append(f"{key} {label}: built by {old.get('built')} -> {phase.get('built')}")
    return why


def options(argv, name, default=None):
    if name in argv:
        at = argv.index(name)
        value = argv[at + 1]
        del argv[at:at + 2]
        return value
    return default


def main():
    argv = sys.argv[1:]
    stored_arg = options(argv, "--stored")
    noise = float(options(argv, "--noise", "10"))
    max_age = float(options(argv, "--max-age", "24"))
    force, check = "--force" in argv, "--check" in argv
    paths = [a for a in argv if not a.startswith("--")]
    if len(paths) != 1:
        raise SystemExit(__doc__)
    result = json.load(open(paths[0]))
    runtime = "runbench" in result
    if runtime:
        stored_path = stored_arg or os.path.join(ROOT, f"docs/benchmarks/runtime-{platform(result).replace(' ', '-').lower()}.json")
        wrong = runtime_wrong(result) if result["runbench"] else ["runbench.py wrote nothing"]
    else:
        stored_path = stored_arg or STORED
        wrong = [f"{key}: {line}" for key, measured in result["results"].items() for line in measured.get("wrong", [])]
    if wrong:
        print("refused: a program did not say what cargo's said\n" + "\n".join(wrong))
        sys.exit(2)
    stored = json.load(open(stored_path)) if os.path.exists(stored_path) else None
    why = (["--force"] if force else []) + (runtime_differences if runtime else differences)(stored, result, noise, max_age)
    changed = bool(why)
    print("\n".join(why[:40]) or f"every number within {noise:.0f}% (or 50 ms) of the stored run")
    if changed and not check:
        os.makedirs(os.path.dirname(stored_path), exist_ok=True)
        if os.path.abspath(paths[0]) != os.path.abspath(stored_path):
            shutil.copyfile(paths[0], stored_path)
        if runtime:
            splice(README, runtime_section(result), RUN_START, RUN_END)
        else:
            splice(README, readme_section(result))
            splice(DOC, doc_section(result))
    if os.environ.get("GITHUB_OUTPUT"):
        open(os.environ["GITHUB_OUTPUT"], "a").write(f"changed={'true' if changed else 'false'}\n")
    print("changed" if changed else "unchanged")
    sys.exit(1 if check and changed else 0)


if __name__ == "__main__":
    main()
