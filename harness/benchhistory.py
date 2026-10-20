#!/usr/bin/env python3
"""benchhistory.py: the benchmark history the site's /bench page charts, and the regression check.

    benchhistory.py build RESULT [--history FILE]
    benchhistory.py live LATENCY... [--history FILE] [--machine NAME]
    benchhistory.py check HISTORY [--last N] [--noise PERCENT] [--report FILE]

`build` appends one line to FILE (default docs/benchmarks/history/linux-x86_64.jsonl) for a
result of bench.py; `live` appends one line (default docs/benchmarks/history/macos-arm64-live.jsonl)
for the files live_latency.py wrote with LATENCY_JSON, one per program. A line keeps the
medians and spreads of each phase, not every sample: the full result is the workflow's
artifact, and the latest one is docs/benchmarks/linux-x86_64.json.

A build line:
    {"suite": "build", "date", "commit", "machine", "cpu", "cores", "runs", "run_url",
     "results": {"<project>/<backend>": {"tag", "store_mb", "runtime_s": {"ra-jit", "cargo debug", "cargo release"},
                 "phases": {"<phase>": {"wall_s", "cargo_s", "min_s", "max_s", "cargo_min_s", "cargo_max_s",
                                        "rss_mb", "built", "link", "compiled", "build_s", "warm_s",
                                        "setups": {"<setup>": seconds}}}}}}
A live line:
    {"suite": "live", "date", "commit", "machine", "battery", "run_url",
     "results": {"<program>": {"<mode>": {"n", "p50_ms", "p90_ms", "p95_ms", "p99_ms", "max_ms"}}}}
Keys a run did not measure are left out.

`check` compares the last line of HISTORY with the lines before it from the same machine (at
most `--last`, default 7): a number regressed when it is above the median of those by more than
its noise band and by more than its floor (50 ms for build times, 1 ms for live latency, 100 MB
of memory, 50 MB of store). The band is `--noise` percent (default 10) or three times the
spread the history shows (1.4826 times the median absolute deviation, as a percent of the
median), whichever is larger; with fewer than three earlier lines it is twice `--noise`. A
phase the daemon built that cargo now builds is a regression whatever the times. The
regressions are written to stdout and, as markdown, to `--report`; the exit status is 1 when
there is one, and 0 with nothing to compare against.
"""
import datetime, json, os, statistics, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BUILD_HISTORY = os.path.join(ROOT, "docs/benchmarks/history/linux-x86_64.jsonl")
LIVE_HISTORY = os.path.join(ROOT, "docs/benchmarks/history/macos-arm64-live.jsonl")
PHASE_FIELDS = ("wall_s", "cargo_s", "rss_mb", "built", "link", "compiled", "build_s", "warm_s",
                "warm_store_s", "warm_store_build_s", "check_s", "edit_s", "edit_cargo_s")
# What is checked, and the least change that counts.
FLOORS = {"wall_s": 0.05, "build_s": 0.05, "warm_s": 0.05, "edit_s": 0.05, "rss_mb": 100, "store_mb": 50,
          "p50_ms": 1.0, "p90_ms": 1.0, "ra-jit": 0.005}


def spread(samples, prefix=""):
    if not samples:
        return {}
    return {f"{prefix}min_s": round(min(samples), 3), f"{prefix}max_s": round(max(samples), 3)}


def build_line(result):
    host = result.get("host") or {}
    line = dict(suite="build", date=result["date"], commit=result.get("commit"), machine=host.get("name"),
                cpu=host.get("cpu"), cores=host.get("cores") or result.get("cores"), runs=result.get("runs"),
                run_url=result.get("run_url") or actions_run(), results={})
    for key, measured in result["results"].items():
        phases = {}
        for label, phase in measured["phases"].items():
            kept = {f: phase[f] for f in PHASE_FIELDS if phase.get(f) is not None}
            kept.update(spread(phase.get("samples_s")))
            kept.update(spread(phase.get("cargo_samples_s"), "cargo_"))
            setups = {name: s["median_s"] for name, s in (phase.get("setups") or {}).items() if s.get("median_s") is not None}
            if setups:
                kept["setups"] = setups
            phases[label] = kept
        entry = dict(tag=measured.get("tag"), phases=phases)
        if measured.get("store_mb") is not None:
            entry["store_mb"] = measured["store_mb"]
        if measured.get("runtime"):
            entry["runtime_s"] = {label: r["median_s"] for label, r in measured["runtime"].items() if isinstance(r, dict) and "median_s" in r}
        line["results"][key] = entry
    return line


def percentile(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(p / 100 * len(values)))]


def live_line(files, machine):
    line = dict(suite="live", results={})
    for path in files:
        latency = json.load(open(path))
        if "modes" not in latency:
            raise SystemExit(f"{path}: not a live_latency.py result with metadata (no `modes`)")
        for field in ("date", "commit", "run_url"):
            if latency.get(field) and not line.get(field):
                line[field] = latency[field]
        host = latency.get("host") or {}
        line.setdefault("machine", machine or host.get("name"))
        line.setdefault("battery", host.get("battery"))
        program = line["results"].setdefault(latency["program"], {})
        for mode, rows in latency["modes"].items():
            totals = [row["total"] for row in rows if isinstance(row.get("total"), (int, float))]
            if totals:
                program[mode] = dict(n=len(totals), **{f"p{p}_ms": round(percentile(totals, p), 2) for p in (50, 90, 95, 99)},
                                     max_ms=round(max(totals), 2))
    line.setdefault("date", time_now())
    line.setdefault("run_url", actions_run())
    return line


def actions_run():
    """The GitHub Actions run this is part of, if it is."""
    if os.environ.get("GITHUB_RUN_ID"):
        return f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}"
    return None


def time_now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def append(path, line):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a") as f:
        f.write(json.dumps(line, separators=(",", ":")) + "\n")
    print(f"appended {line['suite']} {line.get('commit') or ''} {line['date']} to {path}")


def series(line):
    """Every number of a line that is checked, by name, and the `built` of each phase."""
    numbers, built = {}, {}
    for key, entry in line["results"].items():
        if line["suite"] == "build":
            if entry.get("store_mb") is not None:
                numbers[(key, "store", "store_mb")] = entry["store_mb"]
            if (entry.get("runtime_s") or {}).get("ra-jit") is not None:
                numbers[(key, "runtime", "ra-jit")] = entry["runtime_s"]["ra-jit"]
            for label, phase in entry["phases"].items():
                for field in ("wall_s", "rss_mb", "build_s", "warm_s", "edit_s"):
                    if phase.get(field) is not None:
                        numbers[(key, label, field)] = phase[field]
                if phase.get("built"):
                    built[(key, label)] = phase["built"]
        else:
            for mode, stats in entry.items():
                for field in ("p50_ms", "p90_ms"):
                    if stats.get(field) is not None:
                        numbers[(key, mode, field)] = stats[field]
    return numbers, built


def regressions(lines, last, noise):
    now = lines[-1]
    same = [l for l in lines[:-1] if l.get("machine") == now.get("machine") and l.get("cores") == now.get("cores")][-last:]
    if not same:
        return [], 0
    numbers, built = series(now)
    earlier = [series(l) for l in same]
    found = []
    for name, value in sorted(numbers.items()):
        history = [n[name] for n, _ in earlier if name in n]
        if not history:
            continue
        median = statistics.median(history)
        if len(history) >= 3 and median > 0:
            mad = statistics.median(abs(h - median) for h in history) * 1.4826
            band = max(noise, 300 * mad / median)
        else:
            band = 2 * noise
        floor = FLOORS.get(name[2], 0)
        if value > median * (1 + band / 100) and value - median > floor:
            found.append(dict(series=" ".join(name), was=median, now=value, percent=round(100 * (value - median) / median), band=round(band)))
    for name, how in sorted(built.items()):
        was = [b[name] for _, b in earlier if name in b]
        if was and was[-1] == "daemon" and how != "daemon":
            found.append(dict(series=" ".join(name) + " built", was="daemon", now=how, percent=None, band=None))
    return found, len(same)


def report(now, found, compared):
    head = f"{now['suite']} benchmarks on {now.get('machine')}, ra-jit {(now.get('commit') or '?')[:10]}, {now['date']}"
    if now.get("run_url"):
        head += f" ([run]({now['run_url']}))"
    if not found:
        return f"{head}: nothing regressed against the {compared} runs before it."
    lines = [f"{head}: {len(found)} regressed against the median of the {compared} runs before it.", "",
             "| What | Before | Now | Change | Noise band |", "|---|---:|---:|---:|---:|"]
    for f in found:
        change = f"+{f['percent']}%" if f["percent"] is not None else ""
        band = f"{f['band']}%" if f["band"] is not None else ""
        lines.append(f"| {f['series']} | {f['was']} | {f['now']} | {change} | {band} |")
    return "\n".join(lines)


def options(argv, name, default=None):
    if name in argv:
        at = argv.index(name)
        value = argv[at + 1]
        del argv[at:at + 2]
        return value
    return default


def main():
    argv = sys.argv[1:]
    if not argv:
        raise SystemExit(__doc__)
    command = argv.pop(0)
    if command == "build":
        history = options(argv, "--history", BUILD_HISTORY)
        append(history, build_line(json.load(open(argv[0]))))
    elif command == "live":
        history = options(argv, "--history", LIVE_HISTORY)
        machine = options(argv, "--machine")
        append(history, live_line(argv, machine))
    elif command == "check":
        last, noise = int(options(argv, "--last", "7")), float(options(argv, "--noise", "10"))
        out = options(argv, "--report")
        lines = [json.loads(l) for l in open(argv[0]) if l.strip()]
        found, compared = regressions(lines, last, noise)
        text = report(lines[-1], found, compared)
        print(text)
        if out:
            open(out, "w").write(text + "\n")
        sys.exit(1 if found else 0)
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
