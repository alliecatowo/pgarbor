//! `pgarbor version`. Fully implemented, no lease.
//!
//! Human: one line `pgarbor 0.1.0 (PostgreSQL 18.6 compatible; wire protocol 3.0-3.2; container format 1)`.
//! `--json`: [`crate::reports::VersionReport`].
use crate::ctx::{CliResult, Ctx};
use crate::exit::Exit;
use crate::reports::VersionReport;

/// Build the report.
pub fn report() -> VersionReport {
    VersionReport {
        name: "pgarbor",
        version: env!("CARGO_PKG_VERSION"),
        postgres_compat: pgarbor_wire::consts::SERVER_VERSION,
        wire_protocol: "3.0-3.2",
        container_format_version: pgarbor_storage::format::consts::FORMAT_VERSION,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

pub fn run(ctx: &mut Ctx) -> CliResult<Exit> {
    let r = report();
    if ctx.json {
        ctx.emit_json(&r)?;
    } else {
        ctx.out_line(&format!(
            "pgarbor {} (PostgreSQL {} compatible; wire protocol {}; container format {})",
            r.version, r.postgres_compat, r.wire_protocol, r.container_format_version
        ));
    }
    Ok(Exit::OK)
}
