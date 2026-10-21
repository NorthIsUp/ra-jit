//! `ra-jit borrowck [--edition E]`: the diagnostics of single-file crates, one per line of
//! standard input, each a path, written as one JSON object per line on standard output.
//!
//! The standard library is loaded once; each file then takes the place of the one before as
//! the root of a library crate of the given edition, so what is checked again is only what
//! the file changes. `harness/borrowck_ui.py` scores rustc's own `tests/ui` with it.

use std::{
    io::{BufRead as _, Write as _},
    time::Instant,
};

use anyhow::Context as _;
use ide_db::{ChangeWithProcMacros, RootDatabase, base_db::SourceDatabase as _};
use load_cargo::{LoadCargoConfig, ProcMacroServerChoice};
use project_model::{CargoConfig, RustLibSource};
use vfs::{AbsPathBuf, VfsPath};

pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let mut edition = "2015".to_owned();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--edition" => edition = args.next().context("--edition needs a year")?,
            _ => anyhow::bail!("unexpected argument {arg}"),
        }
    }
    let dir =
        std::env::temp_dir().join(format!("ra-jit-borrowck-{}-{edition}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"ui\"\nversion = \"0.0.0\"\nedition = \"{edition}\"\n\n\
             [lib]\npath = \"lib.rs\"\n\n[workspace]\n"
        ),
    )?;
    let root = dir.join("lib.rs");
    std::fs::write(&root, "")?;
    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        all_targets: false,
        ..Default::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: false,
        with_proc_macro_server: ProcMacroServerChoice::None,
        prefill_caches: false,
        num_worker_threads: 1,
        proc_macro_processes: 1,
    };
    let (mut db, vfs, _) =
        load_cargo::load_workspace_at(&dir, &cargo_config, &load_config, &|_| ())?;
    let file = [root.clone(), dunce::canonicalize(&root)?]
        .into_iter()
        .find_map(|it| vfs.file_id(&VfsPath::from(AbsPathBuf::assert_utf8(it))))
        .map(|(file, _)| file)
        .context("the crate root is not loaded")?;
    let config = ide_diagnostics::DiagnosticsConfig::test_sample();
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let test = line?;
        let started = Instant::now();
        let text = std::fs::read_to_string(&test).unwrap_or_default();
        let mut change = ChangeWithProcMacros::default();
        change.change_file(file, Some(load_cargo::source_text(text)));
        db.apply_change(change);
        if let Ok(name) = std::env::var("RA_JIT_BORROWCK_MIR") {
            dump_mir(&db, file, &name);
        }
        let found = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            diagnostics(&db, &config, file)
        }));
        let mut out =
            serde_json::json!({ "path": test, "ms": started.elapsed().as_millis() as u64 });
        match found {
            Ok(found) => out["diagnostics"] = serde_json::Value::Array(found),
            Err(panic) => out["panic"] = crate::panic_message(&*panic).into(),
        }
        let mut stdout = stdout.lock();
        writeln!(stdout, "{out}")?;
        stdout.flush()?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Prints the MIR of each function named `name` at the top of `file`, and of its closures, to
/// stderr: `RA_JIT_BORROWCK_MIR`, for working on the checker.
fn dump_mir(db: &RootDatabase, file: vfs::FileId, name: &str) {
    hir::attach_db(db, || {
        let sema = hir::Semantics::new(db);
        let Some(module) = sema.file_to_module_def(file) else { return };
        for def in module.declarations(db) {
            if let hir::ModuleDef::Function(function) = def
                && function.name(db).as_str() == name
            {
                eprintln!("{}", hir::DefWithBody::from(function).debug_mir(db));
            }
        }
    })
}

/// Every diagnostic rust-analyzer has for `file`, with the line and column (from 1) of where
/// it starts.
fn diagnostics(
    db: &RootDatabase,
    config: &ide_diagnostics::DiagnosticsConfig,
    file: vfs::FileId,
) -> Vec<serde_json::Value> {
    hir::attach_db(db, || {
        let found = ide_diagnostics::full_diagnostics(
            db,
            config,
            &ide_db::assists::AssistResolveStrategy::None,
            file,
        );
        let text = db.file_text(file).text(db);
        found
            .into_iter()
            .filter(|it| it.range.file_id == file)
            .map(|it| {
                let start = usize::from(it.range.range.start()).min(text.len());
                let before = &text[..start];
                let line = before.matches('\n').count() + 1;
                let col = before.rsplit('\n').next().map_or(0, |it| it.chars().count()) + 1;
                serde_json::json!({
                    "code": it.code.as_str(),
                    "severity": format!("{:?}", it.severity),
                    "line": line,
                    "col": col,
                    "message": it.message,
                })
            })
            .collect()
    })
}
