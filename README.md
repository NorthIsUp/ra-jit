# ra-jit

Hot-reload Rust in milliseconds, using rust-analyzer as the compiler front end.

```
nix run github:indexable-inc/ra-jit -- build
```

![Editing the running Game of Life: rule, colours and speed change live](docs/demo.webp)

[mp4](docs/demo.mp4)

Edit the source and the running program changes on the next frame, with its state intact. (The
video is the first ra-jit prototype, a Game of Life; it stays at the tag
[`prototype`](https://github.com/indexable-inc/ra-jit/tree/prototype).)

ra-jit is a newer version of [ra-jit](https://github.com/DioxusLabs/rustd), made specifically
for Indexable's use cases, starting with den's native app. It keeps ra-jit's compiler daemon and
its code generator over rust-analyzer, and adds what those programs need: edits that go into the
program while it runs, executables that keep their entitlements, and fixes for large workspaces.

> [!NOTE]
> **Upstream:** [ra-jit](https://github.com/DioxusLabs/rustd) is where this started and is the
> general-purpose version: a drop-in `cargo ra-jit build/run/test` for real crates. Use it if you
> don't need what is below.

## What ra-jit adds to ra-jit

- **Live edits** ([docs/live.md](docs/live.md)). ra-jit answers an edit with a patch of the
  functions that changed and splices it into the executable on disk. A program that links
  [`ra-jit-live`](crates/ra-jit-live) and calls `Live::start` takes the same patch in memory:
  the daemon lays it out for room the program set aside next to its code, and `Live::poll`
  maps it there and writes a jump over the entry of each function it replaces. A call, a
  vtable or a function pointer a library holds all reach the new code. An edit to a type
  the rest of the program sees can't be applied to the live data, and the program is told to
  start again; so is a program whose executable was linked again. macOS on AArch64.
- **Signed executables.** A package that says
  `[package.metadata.ra-jit] entitlements = "<file>"` gets its executable signed with those
  entitlements by the daemon's own link, and every splice after keeps the signature: a program
  that needs `com.apple.security.cs.allow-jit` (JavaScriptCore's JIT, for one) runs as it
  should from every build.
- **Large workspaces.** The workspace root is found from any package in it, dep-info is read
  against it, a crate is judged stale by when rustc last finished it, a build script's
  checksum freshness no longer stops it, and an `env!` of an unset variable names its crate.

## Use it

`nix run github:indexable-inc/ra-jit -- <cargo-ra-jit command>` runs `cargo ra-jit` with the
pinned toolchain. Without live edits that is all there is to set up. For live edits, in the
program's crate:

```toml
[dependencies]
ra-jit-live = { git = "https://github.com/indexable-inc/ra-jit" }
```

```rust
let mut live = ra_jit_live::Live::start().ok();
loop {
    // Between frames, or wherever nothing of the program's is half done.
    if let Some(outcome) = live.as_mut().and_then(|it| it.poll()) {
        eprintln!("ra-jit: {outcome}");
    }
    // ...
}
```

Run `nix run github:indexable-inc/ra-jit -- build` once, wait for `cargo ra-jit status` to say the program is ready (the
daemon's own link of it), build again to get that link, and start the program. From then on,
`cargo ra-jit build` after each edit puts the change into the running program: the program
notices the patch within 30 ms and applies it in about a millisecond.

## Status

What has been run end to end, on macOS arm64 with Rust 1.99.0: direct calls, trait objects and
function pointers held by a registry take an edit in about a millisecond, an edit undone puts
the old code back, a panic in new code unwinds and is caught, a new function can be called, a
changed struct asks for a restart, a daemon restart or a `cargo build` is noticed, and a signed
executable stays signed through splices.

The program it is made for is den's native app (den-native, about 5,200 functions of its own
and 190,000 instances): its own code edited while it runs, an edit applied in under a
millisecond and about a second from saving the file to the program running the new code. See
[docs/live.md](docs/live.md). What is rough:

- The first edit after the daemon starts writes the whole object (the code of the checkout came
  from the store, and a patch needs code the daemon compiled itself), so the program has to be
  started once more.
- A change to a crate outside the workspace (a path dependency) is not noticed by the daemon:
  build with plain `cargo build` once.
- Live edits are macOS on AArch64 only; elsewhere `Live::start` says so and the splice on disk
  still works.
- Crates that disagree on overflow checks (a `[profile.dev.package]` override) share generic
  instances ra-jit cannot tell apart, so it refuses them and most of what calls them. Make them
  agree for the daemon's build with `--config 'profile.dev.package."<crate>".overflow-checks=true'`.
- A future is laid out by one compiler, so what touches a future of a function left to rustc
  goes to rustc too, and edits there are not taken live. den-native has a handful of such
  functions.

## How it works

ra-jit is one process per user that stays running. It keeps rust-analyzer's analysis of your
workspaces in memory and, in a store on disk, the functions it has compiled. After an edit it
compiles only the functions that changed, with Cranelift or LLVM, and either links them in
front of what rustc built or splices them into the executable that is already there, or, for
a program that links `ra-jit-live`, into the running process.

- **Front end:** rust-analyzer's parser, name resolution, types, MIR, layout and const
  evaluation. Git worktrees of one repository share one database.
- **Code generator:** ours, over rust-analyzer's MIR. Each function is compiled alone, and a
  function compiled once is found again by the hash of what it was compiled from, in any
  workspace on the machine or, through an S3 bucket, on another machine.
- **rustc** still does the first build of a workspace and always builds its dependencies. Once
  the daemon has the workspace, rustc does not run on an edit.
- **Diagnostics:** rust-analyzer's, as `path:line:column: message [E0308]`. There is no borrow
  checker, so keep running `cargo check`; `cargo ra-jit check` is `cargo check`.
- **Toolchain:** pinned to Rust 1.99.0, because the code it links has to agree with rustc on
  symbol names, layout and the unstable ABI.

A function the daemon cannot compile fails the build with the function and the reason; that is
a bug to report. See [docs/architecture.md](docs/architecture.md).

## Quick start

### `cargo ra-jit`

In any cargo project (`nix run github:indexable-inc/ra-jit --` stands in for `cargo ra-jit`):

    cargo ra-jit build
    cargo ra-jit run -- --some-flag
    cargo ra-jit test
    cargo ra-jit check

The first build of a workspace is cargo's own, with ra-jit recording what rustc was asked to
do. It also starts the daemon, which loads the workspace in the background. Until it has,
builds go to cargo and a line on stderr says so:

    ra-jit: the daemon is still loading the workspace (951ms so far); building with cargo

After that an edit is compiled by the daemon and neither cargo nor rustc runs. With
`RA_JIT_FIRST_BUILD=1` the first build is shared: cargo builds the dependencies while the
daemon compiles the workspace's crates and links the program. `cargo ra-jit status`
shows what the daemon holds and how many builds it served or left to cargo, by reason.
`cargo ra-jit stop` ends it; it also exits by itself after 30 minutes without a request.
`cargo ra-jit check` is `cargo check`.

Compiled functions are kept on disk in `~/.cache/ra-jit/store` (8 GiB at most;
`RA_JIT_CACHE=off` turns it off), so a new daemon starts from what the last one built. See [docs/cache.md](docs/cache.md) for the store and the S3 remote.

### Plain `cargo`, switched on globally

Run `ra-jit enable` once. It adds one key to `~/.cargo/config.toml`
(`build.rustc-workspace-wrapper`, pointing at `ra-jit-rustc`), and from then on ordinary
`cargo build`, `cargo run`, `cargo test` and `cargo check` in any workspace go through the
daemon. `ra-jit disable` puts the file back as it was. `ra-jit status` says whether it is on
and how many programs the daemon linked or left to rustc, by reason.

    $ ra-jit enable
    $ cargo run            # the daemon's, once it has loaded the workspace
    $ ra-jit disable

`ra-jit`, `cargo-ra-jit` and `ra-jit-rustc` have to be installed in the same directory. A
wrapper you already use, such as sccache, keeps running. Until the daemon has loaded a
build, rustc links it, and one line on stderr says so. This mode is for agents and scripts that
call cargo themselves; it has been run on Linux and on macOS arm64. On edits it is currently slower than
`cargo ra-jit`, and on some projects slower than cargo alone; see
[docs/cargo.md](docs/cargo.md).

## Results

Linux x86-64, 8 CPUs, Cranelift, seconds, plain cargo first and ra-jit second. Both sides
use the same linker, cargo is warm and incremental, the daemon is warm, and the program
both produce is run and compared. Since these were measured no `cargo check` runs beside
a ra-jit build and nothing is handed back to cargo, so numbers published earlier, which
included a `cargo check` on every edit, no longer apply and have been removed.

Edits in a generated workspace of 200 crates (`harness/genws.py`, timed by
`harness/editrows.py`), median of three:

| Edit | cargo | ra-jit | |
|---|---:|---:|---:|
| A function body in the program's crate | 1.76 | 0.42 | 4.2x |
| A function body in the crate every other depends on | 6.33 | 0.60 | 10.6x |
| Function bodies in 30 crates at once | 6.26 | 0.62 | 10.1x |
| A new function | 2.09 | 0.56 | 3.7x |
| A signature whose callers are in every crate | 13.87 | 10.87 | 1.3x |

The rows were run in that order in one daemon, and the first again after the last (1.79
against 0.45). The last row is barely ahead: the edit changes 2,390 files and the daemon
recompiles 6,806 functions, 3 s of it before compiling anything. That is open.

Real projects, one run each, from `harness/cargo_ra_jit.py --small`. The edit adds a line
to a function:

| Project | No-change build | Build after an edit |
|---|---:|---:|
| ripgrep 15.1.0 | 0.42 / 0.02 | 1.28 / 0.36 |
| fd 10.3.0 | 0.41 / 0.03 | 0.52 / 0.17 |
| hyperfine 1.19.0 | 0.33 / 0.03 | 0.36 / 0.15 |

Most of ra-jit's time in the second column is the client and the link; the daemon's own
time from the edit to a linked ripgrep was 0.05 s when measured alone.

First build from an empty target directory with `RA_JIT_FIRST_BUILD=1`, where cargo builds
the dependencies and ra-jit the workspace's crates, one run each except nushell:

| Project | `cargo build` | `cargo ra-jit build` |
|---|---:|---:|
| ripgrep 15.1.0 | 5.43 | 6.05 |
| fd 10.3.0 | 20.5 | 22.0 |
| nushell 0.108.0 | 42.7, 42.9 | 66.4, 68.1, 89.9 |

ra-jit is behind on all three, and far behind on nushell, where it compiles 165,000
functions. The nushell it links answered 80 of 80 test commands as cargo's does.
bat, just, axum and nushell edits have not been measured again since the change and are
left out.

The same step on macOS arm64 (8-CPU Apple M4 virtual machine), one run each, from
`harness/cargo_ra_jit.py --small`:

| Project | No-change build | Build after an edit, Cranelift | Build after an edit, LLVM |
|---|---:|---:|---:|
| ripgrep 15.1.0 | 0.06 / 0.39 | 2.08 / 1.43 | 0.40 / 0.67 |
| fd 10.3.0 | 0.05 / 0.41 | 0.24 / 0.42 | 0.25 / 0.43 |
| hyperfine 1.19.0 | 0.04 / 0.32 | 0.34 / 0.35 | 0.21 / 0.33 |

ripgrep's edit build varied between runs there (0.52 / 0.73 and 1.08 / 1.30 in two other
Cranelift runs); see [docs/cargo.md](docs/cargo.md#macos-arm64).

On nushell the daemon needs 36 to 51 s to load again after a manifest or build script
changes, and builds are cargo's for that long (measured before the change above).

Correctness is measured by building the same program with rustc and with ra-jit and comparing
what they do. On the last CI run that covered all four native platforms, every function of
the test corpus was compiled by ra-jit under both backends and no type layout differed from
rustc's (822 functions on Linux x86-64, 825 on Linux AArch64, 830 on Windows, 828 on macOS).
Of rustc's own `tests/ui` run-pass tests, 2686 of 2690 behave the same under Cranelift; 1114
were left out, for reasons listed in the results.

## What is supported

|  | Linux x86-64 | Linux AArch64 | Windows x86-64 MSVC | macOS arm64 | wasm32 |
|---|---|---|---|---|---|
| Cranelift | ✅ | ✅ | ✅ | ✅ | ❌ (own lowering) |
| LLVM | ✅ | ✅ | ✅ | ✅ | ❌ |
| Full link | ✅ | ✅ | ✅ | ✅ | ⚠️ older branch only |
| Splice | ✅ | ✅ | ✅ | ✅ not with chained fixups | ⚠️ older branch only |
| Debug info | ✅ | ✅ | ⚠️ lines only | ❓ | ❌ |
| `cargo ra-jit` | ✅ | ❓ | ❌ | ✅ three projects | ❌ |
| Disk store | ✅ | ✅ | ✅ | ✅ | ❓ |
| S3 remote | ✅ not real AWS | ❓ | ❓ | ❓ | ❓ |

✅ means a harness passes there and a report says where it ran. ❓ is unverified, ⚠️ partial. The wasm32 column was
measured on an earlier branch with a nightly toolchain and has not been run since.
[docs/support.md](docs/support.md) has the source of every cell and the same for language and
build-model features.

## What is not supported

These are refused with a reason before the daemon is asked, and cargo builds them instead:

- `lto = true` or `"thin"`;
- through `cargo ra-jit`: `--exclude`, more than one `--target`, `--message-format`, `-Z`
  flags, and a `RUSTC_WORKSPACE_WRAPPER` that names another wrapper;
- test executables built without incremental compilation or above `opt-level = 0`, and unit
  tests of a library other tests link as rustc built it;
- any edit to a manifest, `Cargo.lock`, cargo configuration, a build script or a proc macro:
  that build is cargo's, and the daemon reloads afterwards.

These fail the build, since nothing else builds them: a few language features, among them
`sym` operands in `asm!` that is not naked, some variadic function definitions, and vectors
in a C ABI.

Not there at all: rustc's diagnostics and the borrow checker, optimised output comparable to `--release`, types and
variables in Windows debug info, and targets other than the ones in the table.

## Credits

ra-jit includes code from [rustd](https://github.com/DioxusLabs/rustd) (DioxusLabs), commit
`159f693`; the daemon and the code generator are theirs, and we thank them. The section
below says what else it stands on.

## rust-analyzer

ra-jit's front end is rust-analyzer: its parser, name resolution, type inference, MIR,
monomorphisation, layout and const evaluation, and its cargo workspace loading. ra-jit adds a
code generator on top of rust-analyzer's MIR and the daemon around it. rust-analyzer is
included as a submodule. It points at the `rust-analyzer` branch of this repository, which is
upstream rust-analyzer at the commit ra-jit forked from with ra-jit's changes committed on top. None of this would exist without the rust-analyzer team's work.
ra-jit is not part of the rust-analyzer project and is not endorsed by it; please do not
report ra-jit problems there.

Type layout is computed with rustc's own `rustc_abi` crate, copied unchanged from the 1.99.0
tag into `vendor/rustc_abi`.

## Licence

MIT or Apache-2.0, at your option, the same as rust-analyzer and rustc. See
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).

## Documentation

- [docs/cargo.md](docs/cargo.md): using `cargo ra-jit`
- [docs/support.md](docs/support.md): the support matrix
- [docs/platforms.md](docs/platforms.md): the state of each platform
- [docs/cache.md](docs/cache.md): the disk store and the S3 remote
- [docs/architecture.md](docs/architecture.md): how it works
- [docs/results.md](docs/results.md): measurements and where each came from
- [docs/harness.md](docs/harness.md): the test harnesses
- [docs/fuzzing.md](docs/fuzzing.md): the differential fuzzer
- [docs/contributing.md](docs/contributing.md): layout, changing rust-analyzer, moving to a newer one
- [SECURITY.md](SECURITY.md): reporting a problem, and what the daemon and a shared store trust
- `docs/reports/`: the working reports the above were written from
