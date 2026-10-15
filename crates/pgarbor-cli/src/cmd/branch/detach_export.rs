//! `pgarbor branch detach` and `branch export` (lease CLI-21; BRANCH-09, CLI-06/12 in the seed DAG,
//! RFC 001 sections 4.2-4.3): produce an independent standalone `.pga` containing one branch.
//!
//! The command contract and output formats are described in the module documentation frozen in
//! the CLI skeleton.
use std::path::{Path, PathBuf};

use pgarbor_branches::{DetachOptions, DetachStrategy};

use crate::cli::{BranchDetachArgs, BranchExportArgs, StrategyArg};
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::{open, paths};

fn strategy(arg: StrategyArg) -> DetachStrategy {
    match arg {
        StrategyArg::Auto => DetachStrategy::Auto,
        StrategyArg::Reflink => DetachStrategy::ReflinkRequired,
        StrategyArg::Copy => DetachStrategy::Copy,
    }
}

fn destination_path(path: &Path) -> CliResult<PathBuf> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(CliError::not_found(format!("output directory \"{}\" does not exist", parent.display())));
    }
    Ok(path.to_path_buf())
}

// Canonicalize the existing parent when the destination itself does not exist, so aliases such as
// `./db.pga` and `/cwd/db.pga` cannot accidentally identify the source file.
fn comparable_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    match (std::fs::canonicalize(parent), path.file_name()) {
        (Ok(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

fn validate_destination(source: &Path, out: &Path, overwrite: bool) -> CliResult<PathBuf> {
    let out = destination_path(out)?;
    if comparable_path(source) == comparable_path(&out) {
        return Err(CliError::usage("output file must differ from the source database"));
    }
    if !overwrite && out.exists() {
        return Err(CliError::new(Exit::EXISTS, format!("file \"{}\" already exists", out.display())));
    }
    Ok(out)
}

fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn emit_report(ctx: &mut Ctx, report: &pgarbor_branches::DetachReport, action: &str, source_name: &str) -> CliResult<Exit> {
    if ctx.json {
        ctx.emit_json(report)?;
    } else {
        let filename = report.path.file_name().unwrap_or(report.path.as_os_str()).to_string_lossy();
        let method = match report.strategy_used {
            pgarbor_branches::StrategyUsed::Reflink => "reflink",
            pgarbor_branches::StrategyUsed::Copy => "copy",
        };
        let mut line = format!(
            "{action} branch \"{source_name}\" -> {filename} ({method}, {}, id {})",
            format_bytes(report.file_len),
            report.new_db_uuid
        );
        if let Some(reason) = &report.fallback_reason {
            line.push_str(&format!("; copied because {reason}"));
        }
        ctx.out_line(&line);
    }
    Ok(Exit::OK)
}

pub fn detach(ctx: &mut Ctx, args: &BranchDetachArgs) -> CliResult<Exit> {
    let source = paths::resolve_db(None, args.db.db.as_deref())?;
    let out = validate_destination(&source, &args.out, args.overwrite)?;
    let db = open::open_database(&source, &open::OpenSpec::existing())?;
    let options = DetachOptions {
        strategy: strategy(args.strategy),
        target_name: args.target_name.clone(),
        include_snapshots: args.include_snapshots,
        overwrite: args.overwrite,
        os_filesystem: true,
        ..DetachOptions::default()
    };
    if options.strategy == DetachStrategy::Copy {
        ctx.info(&format!("exporting branch \"{}\" ...", args.name));
    }
    let result = db.detach_branch(&args.name, &out, &options);
    let close_result = db.close();
    let report = result.map_err(CliError::from)?;
    close_result.map_err(CliError::from)?;
    emit_report(ctx, &report, "detached", &args.name)
}

pub fn export(ctx: &mut Ctx, args: &BranchExportArgs) -> CliResult<Exit> {
    let source = paths::resolve_db(None, args.db.db.as_deref())?;
    let out = validate_destination(&source, &args.out, args.overwrite)?;
    let db = open::open_database(&source, &open::OpenSpec::existing())?;
    let options = DetachOptions {
        strategy: DetachStrategy::Copy,
        target_name: args.target_name.clone(),
        include_snapshots: args.include_snapshots,
        overwrite: args.overwrite,
        os_filesystem: true,
        ..DetachOptions::default()
    };
    ctx.info(&format!("exporting branch \"{}\" ...", args.name));
    let result = db.export_branch(&args.name, &out, &options);
    let close_result = db.close();
    let report = result.map_err(CliError::from)?;
    close_result.map_err(CliError::from)?;
    emit_report(ctx, &report, "detached", &args.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_strategy_arguments() {
        assert_eq!(strategy(StrategyArg::Auto), DetachStrategy::Auto);
        assert_eq!(strategy(StrategyArg::Reflink), DetachStrategy::ReflinkRequired);
        assert_eq!(strategy(StrategyArg::Copy), DetachStrategy::Copy);
    }

    #[test]
    fn formats_binary_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
    }
}
