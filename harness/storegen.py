#!/usr/bin/env python3
"""storegen.py: how much of the project a user works on every day the store still holds when
other projects share it and it is kept small.

    storegen.py --max SIZE [--rounds N] [--hot ripgrep] [--others fd,bat,hyperfine,just,tokei] [--out FILE]

One store, one `RA_JIT_HOME`, a copy of each project. Sessions go hot, other, hot, other...:
each one stops the daemon, edits the project's first `edits` line with a number of its own (so
every session adds code the store has never seen), builds with `cargo ra-jit build` (cold:
the daemon loads the workspace and compiles every function, from the store where it can),
waits for the daemon to have it loaded, makes a second edit and builds again. What the
daemon says of the store (`store: N functions found, M compiled and added`) is summed per
session. The number that matters is the share of the hot project's functions that came from
the store in its sessions after its first: a function compiled there is one the store lost.

Prints a line per session and writes the sessions as JSON to `--out`.
"""
import argparse, json, os, re, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from cargo_ra_jit import PROJECTS, RA_JIT, Side, base_env, fetch, wait_warm

SAID = re.compile(r"^store: (\d+) functions found, (\d+) compiled and added(?:, (\d+) copied forward)?", re.M)


def logs(home):
    """Every log under the daemon's home, by what it says now. A worker's log
    (`spares/N/log`) is started over when its directory is taken again, so a log is
    compared by its text, not its size."""
    texts = {}
    for root, _, names in os.walk(home):
        for name in names:
            if name == "log" or name.endswith(".log"):
                path = os.path.join(root, name)
                with open(path, errors="replace") as f:
                    texts[path] = f.read()
    return texts


def since(before, home):
    """What the logs under `home` say that they did not when `before` was taken."""
    text = ""
    for path, now in logs(home).items():
        old = before.get(path, "")
        text += now[len(old):] if now.startswith(old) else now
    return text


def edit(ws, spec, original, n):
    """The project as it came, with its first edit made and numbered `n`."""
    path, old, new = spec["edits"][0]
    full = os.path.join(ws, path)
    text = original.setdefault(full, open(full).read())
    assert old in text, f"{old!r} is not in {full}"
    open(full, "w").write(text.replace(old, new.replace('");', f' {n}");'), 1))


def pack_bytes(store, env):
    ran = subprocess.run([RA_JIT, "cache", "stats"], capture_output=True, text=True, env=env)
    found = re.search(r"^pack bytes: (\d+)", ran.stdout, re.M)
    return int(found[1]) if found else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--max", required=True)
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--hot", default="ripgrep")
    parser.add_argument("--others", default="fd,bat,hyperfine,just,tokei")
    parser.add_argument("--out")
    parser.add_argument("--work")
    args = parser.parse_args()
    work = args.work or tempfile.mkdtemp(prefix="storegen-")
    home = os.path.join(work, "home")
    env = dict(base_env(home), RA_JIT_CACHE_MAX=args.max)
    store = env["RA_JIT_CACHE"]
    others = args.others.split(",")
    sides, original = {}, {}
    for name in [args.hot, *others]:
        ws = os.path.realpath(os.path.join(work, name))
        if not os.path.isdir(ws):
            shutil.copytree(fetch(name, PROJECTS[name]), ws, symlinks=True, ignore=shutil.ignore_patterns("target"))
        project_env = dict(env, **PROJECTS[name].get("env", {}))
        subprocess.run(["cargo", "fetch"], cwd=ws, env=project_env, capture_output=True)
        sides[name] = (ws, Side(ws, project_env, True))
    order = [it for _ in range(args.rounds) for other in others for it in (args.hot, other)]
    sessions, n = [], 0
    for name in order:
        ws, side = sides[name]
        side.cargo("stop")
        before = logs(home)
        n += 1
        edit(ws, PROJECTS[name], original, n)
        took, ran = side.cargo("build", *PROJECTS[name].get("build", []))
        _, states = wait_warm(side, ws)
        n += 1
        edit(ws, PROJECTS[name], original, n)
        side.cargo("build", *PROJECTS[name].get("build", []))
        side.cargo("stop")
        said = SAID.findall(since(before, home))
        found = sum(int(it[0]) for it in said)
        added = sum(int(it[1]) for it in said)
        copied = sum(int(it[2] or 0) for it in said)
        row = dict(project=name, found=found, added=added, copied=copied, packs=pack_bytes(store, env),
                   states="; ".join(states)[:100], cold=round(took, 1))
        sessions.append(row)
        share = found / (found + added) if found + added else 0
        print(f"{len(sessions):3} {name:10} found {found:7} added {added:7} copied {copied:6} "
              f"hit {share:6.1%} packs {row['packs']}  {row['states']}", flush=True)
    sides[args.hot][1].cargo("stop")
    hot = [it for it in sessions if it["project"] == args.hot][1:]
    found, added = sum(it["found"] for it in hot), sum(it["added"] for it in hot)
    allf, alla = sum(it["found"] for it in sessions), sum(it["added"] for it in sessions)
    summary = dict(max=args.max, hot_found=found, hot_added=added,
                   hot_hit=found / (found + added) if found + added else None,
                   all_found=allf, all_added=alla, copied=sum(it["copied"] for it in sessions))
    print(json.dumps(summary), flush=True)
    if args.out:
        json.dump(dict(summary=summary, sessions=sessions), open(args.out, "w"), indent=1)


if __name__ == "__main__":
    main()
