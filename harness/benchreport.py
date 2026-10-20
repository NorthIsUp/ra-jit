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
A result in which a program said something else than cargo's (`wrong`) is refused: exit 2,
nothing written. `--check` writes nothing and exits 1 when it would have written.

The last line says `changed` or `unchanged`, and so does `changed=true|false` in
$GITHUB_OUTPUT when that is set.
"""
import datetime, json, os, re, shutil, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
STORED = os.path.join(ROOT, "docs/benchmarks/linux-x86_64.json")
README = os.path.join(ROOT, "README.md")
DOC = os.path.join(ROOT, "docs/benchmarks.md")
START, END = "<!-- bench:start -->", "<!-- bench:end -->"
REPO = "https://github.com/indexable-inc/ra-jit"
FLOOR_S = 0.05

# The order of the projects and phases in the tables, and what the README calls each edit.
PROJECTS = ["ripgrep", "fd", "bat", "hyperfine", "just", "axum", "nushell", "helix", "bevy", "zed", "cargo"]
EDITS = {
    "body": "Edit a function",
    "leaf": "Edit a widely used function",
    "signature": "Change a signature",
    "generic": "Edit a generic function",
    "newfn": "Add a function",
    "struct": "Add a struct field",
    "no-op": "Nothing changed",
}
DOC_PHASES = ["body", "leaf", "signature", "generic", "newfn", "struct", "no-op"]
# What docs/benchmarks.md calls each setup of bench.py, in the order of its table.
SETUPS = {
    "check": "`cargo check`",
    "mold": "mold",
    "wild": "wild",
    "threads8": "`-Zthreads=8`",
    "nightly": "nightly",
    "cranelift": "nightly, Cranelift",
    "fastest": "nightly, Cranelift, wild, `-Zthreads=8`",
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


def provenance(result):
    """Who ran it: the CI run, or a person with the same harness."""
    if result.get("run_url"):
        return f"Measured by [CI]({result['run_url']}) on the dedicated benchmark machine, which re-runs it after pushes to main."
    return "Measured by hand with `harness/bench.py`; the `bench-readme` workflow re-runs it on the dedicated benchmark machine."


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
            for label in DOC_PHASES:
                phase = measured["phases"].get(label)
                if phase is None:
                    continue
                lines.append(f"| {project_name(key, measured)} | {EDITS[label]} | {seconds(phase['cargo_s'])} | {seconds(phase['wall_s'])} | "
                             f"{speedup(phase['cargo_s'], phase['wall_s'])} | {spread(phase.get('cargo_samples_s'))} | {spread(phase.get('samples_s'))} | "
                             f"{phase['built'][:80]} | {phase.get('link') or ''} |")
        cold_runs = result.get("cold_runs", 1)
        lines += ["", f"#### Cold, {BACKEND_NAMES[backend]}", "",
                  "From an empty target directory, crates already downloaded, " + ("one run." if cold_runs == 1 else
                  f"median of {cold_runs} runs, each from nothing.") + " `cargo ra-jit build` is cargo's own build (the daemon is "
                  "not loaded yet); the daemon then loads the workspace and builds it once, and edits are fast from then on. "
                  "Warm store: the same in another checkout of the same commit, with the store the first one filled.", "",
                  "| Project | `cargo build` | `cargo ra-jit build` | Daemon ready, empty store | Daemon ready, warm store | "
                  "Functions compiled | Daemon memory | Store |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|"]
        for key in keys:
            measured = result["results"][key]
            cold = measured["phases"].get("cold")
            if cold:
                ready = lambda total, samples: seconds(total) + (f" ({spread(samples)})" if samples and len(samples) > 1 else "")
                store = f"{measured['store_mb']:,} MB" if measured.get("store_mb") is not None else "-"
                lines.append(f"| {project_name(key, measured)} | {ready(cold['cargo_s'], cold.get('cargo_samples_s'))} | "
                             f"{ready(cold.get('build_s'), cold.get('build_samples_s'))} | {ready(cold.get('wall_s'), cold.get('samples_s'))} | "
                             f"{ready(cold.get('warm_store_s'), cold.get('warm_store_samples_s'))} | "
                             f"{cold.get('compiled', 0):,} | {cold.get('rss_mb', 0):,} MB | {store} |")
        lines += setups_table(result, keys, backend) + tests_table(result, keys, backend)
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


def setups_table(result, keys, backend):
    """The same steps built the fastest ways there are besides ra-jit, for the projects that had them."""
    keys = [k for k in keys if any(p.get("setups") for p in result["results"][k]["phases"].values())]
    if not keys:
        return []
    names = [s for s in SETUPS if any(s in (p.get("setups") or {}) for k in keys for p in result["results"][k]["phases"].values())]
    nightly = (result.get("host") or {}).get("nightly")
    lines = ["", f"#### Against the fastest setups, {BACKEND_NAMES[backend]}", "",
             "The same steps, each built after plain cargo by the fastest ways to build Rust there already are, each into a "
             "target directory of its own, median of the same runs. Plain cargo is incremental, with the default linker. "
             f"mold and wild link instead of GNU ld; `-Zthreads=8` is the parallel front end on the same compiler; the nightly ({nightly or 'see the JSON'}) "
             "is there to tell its own difference from Cranelift's (rustc_codegen_cranelift comes only with nightlies); `cargo check` "
             "builds no code, so nothing can be run after it. The fastest of each row is in bold; \"failed\" is a setup that could "
             "not build the project, and is not timed after that.", "",
             "| Project | Step | cargo | " + " | ".join(SETUPS[n] for n in names) + " | ra-jit |",
             "|---|---|---:|" + "---:|" * (len(names) + 1)]
    for key in keys:
        measured = result["results"][key]
        for label in ["cold", *DOC_PHASES]:
            phase = measured["phases"].get(label)
            if not phase or not phase.get("setups"):
                continue
            ours = phase["wall_s"] if label != "cold" else phase.get("wall_s")
            times = {"cargo": phase["cargo_s"], "ra-jit": ours}
            times.update({n: phase["setups"].get(n, {}).get("median_s") for n in names})
            # `cargo check` builds nothing to run, so it is not what the fastest is chosen from.
            fastest = min((v, n) for n, v in times.items() if v is not None and n != "check")[1]
            cell = lambda n: ("failed" if (phase["setups"].get(n) or {}).get("failed") else "-") if times[n] is None else (
                f"**{seconds(times[n])}**" if n == fastest else seconds(times[n]))
            step = "Cold, until the daemon is ready" if label == "cold" else EDITS[label]
            lines.append(f"| {project_name(key, measured)} | {step} | {cell('cargo')} | " + " | ".join(cell(n) for n in names) + f" | {cell('ra-jit')} |")
    failures = sorted({f"{project_name(k, result['results'][k])}, {SETUPS[n]}: {s['failed']}" for k in keys
                       for p in result["results"][k]["phases"].values() for n, s in (p.get("setups") or {}).items() if s.get("failed")})
    if failures:
        lines += [""] + [f"- {f}" for f in failures]
    return lines


def tests_table(result, keys, backend):
    keys = [k for k in keys if result["results"][k]["phases"].get("test")]
    if not keys:
        return []
    lines = ["", f"#### Test suites, {BACKEND_NAMES[backend]}", "",
             "`cargo ra-jit test` and `cargo test` of the same tests, median of three: after the edit of a widely used function "
             "(the file named), and again with nothing changed. Each includes running the tests. First runs: the three untimed "
             "runs of `cargo ra-jit test` before them (the first is handed to cargo, the daemon builds the test targets at the "
             "second). Every test has to end the same way under both, every time.", "",
             "| Project | Command | Tests | Same outcome | Edited | Edit, then test: cargo | ra-jit | | Built | Nothing changed: cargo | ra-jit | First runs of ra-jit |",
             "|---|---|---:|---|---|---:|---:|---:|---|---:|---:|---|"]
    for key in keys:
        test = result["results"][key]["phases"]["test"]
        if "edit_s" not in test:
            continue
        lines.append(f"| {project_name(key, result['results'][key])} | `cargo {' '.join(test['command'])}` | {test['tests']:,} | "
                     f"{'yes' if test['same'] else 'no: ' + (test.get('difference') or '')[:120]} | `{test['edited']}` | "
                     f"{seconds(test['edit_cargo_s'])} | {seconds(test['edit_s'])} | {speedup(test['edit_cargo_s'], test['edit_s'])} | {test['built'][:60]} | "
                     f"{seconds(test['cargo_s'])} | {seconds(test['wall_s'])} | {', '.join(seconds(s) for s in test['first_s'])} |")
    return lines


def splice(path, section):
    text = open(path).read()
    pattern = re.compile(re.escape(START) + ".*?" + re.escape(END), re.S)
    if not pattern.search(text):
        raise SystemExit(f"{path} has no {START} ... {END} section")
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
    stored_path = options(argv, "--stored", STORED)
    noise = float(options(argv, "--noise", "10"))
    max_age = float(options(argv, "--max-age", "24"))
    force, check = "--force" in argv, "--check" in argv
    paths = [a for a in argv if not a.startswith("--")]
    if len(paths) != 1:
        raise SystemExit(__doc__)
    result = json.load(open(paths[0]))
    wrong = [f"{key}: {line}" for key, measured in result["results"].items() for line in measured.get("wrong", [])]
    if wrong:
        print("refused: a program did not say what cargo's said\n" + "\n".join(wrong))
        sys.exit(2)
    stored = json.load(open(stored_path)) if os.path.exists(stored_path) else None
    why = (["--force"] if force else []) + differences(stored, result, noise, max_age)
    changed = bool(why)
    print("\n".join(why[:40]) or f"every number within {noise:.0f}% (or 50 ms) of the stored run")
    if changed and not check:
        os.makedirs(os.path.dirname(stored_path), exist_ok=True)
        if os.path.abspath(paths[0]) != os.path.abspath(stored_path):
            shutil.copyfile(paths[0], stored_path)
        splice(README, readme_section(result))
        splice(DOC, doc_section(result))
    if os.environ.get("GITHUB_OUTPUT"):
        open(os.environ["GITHUB_OUTPUT"], "a").write(f"changed={'true' if changed else 'false'}\n")
    print("changed" if changed else "unchanged")
    sys.exit(1 if check and changed else 0)


if __name__ == "__main__":
    main()
