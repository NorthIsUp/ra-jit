#!/usr/bin/env python3
"""Cargo unit graph -> per-unit Nix build description, one graph per nix system.

  tools/unit_graph.py   # cargo --unit-graph per target triple + cargo metadata -> units.json (committed)

`units.json` is read by nix/units.nix, which picks `systems.<nix system>` and makes one derivation per
unit, calling rustc directly (tools/unit_build.py). Platform-specific dependencies (num_threads -> libc
on apple, inotify on linux, ...) differ per target, so each nix system gets its own graph. Rerun whenever
Cargo.toml, Cargo.lock or a feature set changes; nix/units.nix refuses to evaluate when units.json is
stale against Cargo.lock / Cargo.toml.

Python 3.12+, stdlib only. Every step has its own timeout; failures name the step, command, exit code
and stderr tail.
"""

import argparse
import asyncio
import hashlib
import json
import os
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# nix system -> rust target triple. Builds are native, so the triple is also the host of that system.
TARGETS = {
    "x86_64-linux": "x86_64-unknown-linux-gnu",
    "aarch64-linux": "aarch64-unknown-linux-gnu",
    "aarch64-darwin": "aarch64-apple-darwin",
    "x86_64-darwin": "x86_64-apple-darwin",
}
UNITS_JSON = ROOT / "units.json"


class StepError(Exception):
    pass


async def run_step(step: str, cmd: list[str], timeout: float, env: dict | None = None) -> str:
    full_env = {**os.environ, **(env or {})}
    try:
        async with asyncio.timeout(timeout):
            proc = await asyncio.create_subprocess_exec(
                *cmd, cwd=ROOT, env=full_env,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            )
            out, err = await proc.communicate()
    except TimeoutError:
        raise StepError(f"step '{step}' timed out after {timeout:.0f}s\n  command: {' '.join(cmd)}") from None
    if proc.returncode != 0:
        tail = "\n".join(err.decode(errors="replace").strip().splitlines()[-30:])
        raise StepError(f"step '{step}' failed\n  command: {' '.join(cmd)}\n  exit code: {proc.returncode}\n  stderr tail:\n{tail}")
    return out.decode()


def toolchain_prefix() -> list[str]:
    """Run cargo from the flake dev shell (pinned rustc/cargo) unless we are already in it."""
    if os.environ.get("IN_NIX_SHELL"):
        return []
    return ["nix", "develop", str(ROOT), "--command"]


async def fetch_graphs() -> tuple[dict[str, dict], dict]:
    pre = toolchain_prefix()
    # -Z unstable-options needs the pinned nightly from the flake dev shell (no RUSTC_BOOTSTRAP).
    await run_step("cargo fetch", [*pre, "cargo", "fetch", "--locked"], 900)
    async with asyncio.TaskGroup() as tg:
        graphs = {
            system: tg.create_task(run_step(
                f"cargo unit-graph {triple}",
                [*pre, "cargo", "build", "--unit-graph", "-Z", "unstable-options", "--release", "--locked", "--target", triple],
                300))
            for system, triple in TARGETS.items()
        }
        t_meta = tg.create_task(run_step(
            "cargo metadata", [*pre, "cargo", "metadata", "--format-version", "1", "--locked"], 300))
    return {s: json.loads(t.result()) for s, t in graphs.items()}, json.loads(t_meta.result())


def sha256(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def key_of(pkg_id: str) -> str:
    # `<source>#name@version` (or `<source>#version` when the name is the path's last segment).
    src, _, frag = pkg_id.partition("#")
    if "@" in frag:
        return frag
    return f"{src.rstrip('/').rsplit('/', 1)[-1].split('?')[0]}@{frag}"


def build_packages(meta: dict) -> dict[str, dict]:
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    checksums = {(p["name"], p["version"]): p.get("checksum") for p in lock["package"]}
    packages: dict[str, dict] = {}
    for p in meta["packages"]:
        key = f"{p['name']}@{p['version']}"
        if key in packages:
            raise StepError(f"step 'nix': two packages named {key}; the key scheme needs the source too")
        mdir = Path(p["manifest_path"]).parent
        if p["source"] and p["source"].startswith("registry+"):
            cks = checksums[(p["name"], p["version"])]
            source = {"kind": "registry", "checksum": cks}
        elif p["source"] is None:
            source = {"kind": "path", "dir": str(mdir.relative_to(ROOT)) if mdir != ROOT else "."}
        else:
            raise StepError(f"step 'nix': unsupported source {p['source']} for {key} (only registry and path)")
        packages[key] = {
            "name": p["name"], "version": p["version"], "source": source, "links": p.get("links"),
            "authors": ":".join(p["authors"]), "description": p["description"] or "",
            "homepage": p["homepage"] or "", "repository": p["repository"] or "",
            "license": p["license"] or "", "license_file": p["license_file"] or "",
            "rust_version": p["rust_version"] or "", "readme": p["readme"] or "",
            "manifest_dir": str(mdir),
        }

    return packages


def build_system(system: str, graph: dict, packages: dict[str, dict], triple: str) -> dict:
    units = []
    for u in graph["units"]:
        pkg = key_of(u["pkg_id"])
        if pkg not in packages:
            raise StepError(f"step 'nix': unit package {u['pkg_id']} is not in cargo metadata")
        t = u["target"]
        # platform None = host unit (build script / proc-macro); otherwise the target triple. Builds are
        # native, so both are the same machine in nix.
        if u["platform"] not in (None, triple):
            raise StepError(f"step 'nix {system}': unit platform {u['platform']} is neither host nor {triple}")
        if u["mode"] not in ("build", "run-custom-build"):
            raise StepError(f"step 'nix': unsupported unit mode {u['mode']}")
        pdir = Path(packages[pkg]["manifest_dir"])
        pr = u["profile"]
        units.append({
            "pkg": pkg,
            "target": t["name"],
            "kind": t["kind"][0],
            "crate_type": t["crate_types"][0],
            "mode": u["mode"],
            "edition": t["edition"],
            "src": str(Path(t["src_path"]).relative_to(pdir)),
            "features": sorted(u["features"]),
            "profile": {k: pr[k] for k in ("name", "opt_level", "debuginfo", "debug_assertions", "overflow_checks", "panic", "codegen_units")},
            "deps": [{"i": d["index"], "name": d["extern_crate_name"]} for d in u["dependencies"]],
        })
    return {"units": units, "roots": graph["roots"]}


def write_units(graphs: dict[str, dict], meta: dict) -> None:
    packages = build_packages(meta)
    systems = {system: build_system(system, g, packages, TARGETS[system]) for system, g in graphs.items()}
    for p in packages.values():
        del p["manifest_dir"]  # absolute, host specific
    out = {
        "version": 2,
        "inputs": {n: sha256(ROOT / n) for n in ("Cargo.lock", "Cargo.toml")},
        "packages": packages,
        "systems": systems,
    }
    UNITS_JSON.write_text(json.dumps(out, indent=1, sort_keys=True) + "\n")
    counts = ", ".join(f"{s}: {len(v['units'])}" for s, v in systems.items())
    print(f"wrote {UNITS_JSON.name}: {len(packages)} packages; units per system: {counts}")


async def main() -> None:
    argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter).parse_args()
    graphs, meta = await fetch_graphs()
    write_units(graphs, meta)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except* StepError as eg:
        for e in eg.exceptions:
            print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
