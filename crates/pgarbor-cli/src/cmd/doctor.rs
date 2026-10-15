//! `pgarbor doctor` (lease CLI-16): explain problems; never modifies anything.
//!
//! Runs checks in a stable order and reports repair hints. The database is always opened
//! read-only; the only filesystem write is a temporary probe in its parent directory.
use std::fs::{self, OpenOptions};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::FileArgs;
use crate::ctx::{CliError, CliResult, Ctx};
use crate::exit::Exit;
use crate::open::open_container_readonly;
use crate::reports::{DoctorCheck, DoctorOutput};
use pgarbor_storage::{NoNestedRoots, VerifyLevel};

fn check(name: &str, status: &str, message: impl Into<String>, hint: Option<&str>) -> DoctorCheck {
    DoctorCheck { name: name.to_string(), status: status.to_string(), message: message.into(), hint: hint.map(str::to_string) }
}

fn file_check(path: &Path) -> DoctorCheck {
    match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => check("file", "fail", format!("file does not exist: {}", path.display()), Some("check the database path or create it with pgarbor init")),
        Err(e) => check("file", "fail", format!("could not inspect file: {e}"), Some("check file permissions and the path")),
        Ok(m) if !m.is_file() => check("file", "fail", "path is not a regular file", Some("pass the path to a PGArbor database file")),
        Ok(m) => {
            if let Err(e) = OpenOptions::new().read(true).open(path) {
                return check("file", "fail", format!("file is not readable: {e}"), Some("check file permissions"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if m.permissions().mode() & 0o222 == 0 {
                    return check("file", "warn", "file is read-only; serve --read-only only", Some("make the file writable before running a writable server"));
                }
            }
            check("file", "ok", "file exists and is readable", None)
        }
    }
}

fn directory_check(path: &Path) -> DoctorCheck {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = path.file_name().unwrap_or_else(|| std::ffi::OsStr::new("database"));
    let mut probe_name = name.to_os_string();
    probe_name.push(".doctor-probe");
    let probe = parent.join(probe_name);
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            match fs::remove_file(&probe) {
                Ok(()) => check("directory", "ok", "parent directory is writable", None),
                Err(e) => check("directory", "warn", format!("could not remove directory probe: {e}"), Some("remove the doctor probe file after checking directory permissions")),
            }
        }
        Err(e) => check("directory", "warn", format!("parent directory is not writable: {e}"), Some("check parent directory permissions; writable access is needed for compact/export/serve metadata")),
    }
}

fn leftovers_check(path: &Path) -> DoctorCheck {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let base = path.file_name().unwrap_or_else(|| std::ffi::OsStr::new("database")).to_string_lossy();
    let mut found = Vec::new();
    for suffix in [".compact-tmp", ".export-tmp"] {
        let candidate = parent.join(format!("{base}{suffix}"));
        if candidate.exists() { found.push(candidate.display().to_string()); }
    }
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".detach-tmp") { found.push(entry.path().display().to_string()); }
        }
    }
    if found.is_empty() { check("leftovers", "ok", "no interrupted-operation files found", None) }
    else { check("leftovers", "warn", format!("leftover temporary files: {}", found.join(", ")), Some("left over from an interrupted compact/export; safe to delete when no pgarbor is running")) }
}

fn superblock_check(path: &Path) -> DoctorCheck {
    match open_container_readonly(path, VerifyLevel::Roots) {
        Err(e) => {
            let msg = e.message.clone();
            if e.exit == Exit::LOCKED {
                return check("superblock", "skip", "could not read superblock while database is locked", Some("stop the process holding the database lock before inspecting the file"));
            }
            check("superblock", "fail", msg, Some("check the file format; upgrade pgarbor if the format is newer"))
        }
        Ok(container) => {
            let rec = container.recovery_report();
            let mut notes = vec![format!("valid superblock in slot {}", rec.chosen_slot)];
            if rec.fell_back_to_older_slot { notes.push("recovered using the older valid superblock slot".to_string()); }
            if rec.truncated_tail_bytes > 0 { notes.push("uncommitted tail will be truncated on next writable open".to_string()); }
            let warn = rec.fell_back_to_older_slot || rec.truncated_tail_bytes > 0;
            check("superblock", if warn { "warn" } else { "ok" }, notes.join("; "), if warn { Some("make a writable open to complete recovery after ensuring the database is not in use") } else { None })
        }
    }
}

fn integrity_check(path: &Path) -> DoctorCheck {
    match open_container_readonly(path, VerifyLevel::None) {
        Err(e) if e.exit == Exit::LOCKED => check("integrity", "skip", "integrity check skipped because the database is locked", Some("stop the process holding the database lock before verifying")),
        Err(e) => check("integrity", "fail", e.message, Some("run pgarbor verify and restore from a known-good copy if needed")),
        Ok(container) => match container.verify(&NoNestedRoots, VerifyLevel::Full) {
            Err(e) => check("integrity", "fail", e.message, Some("run pgarbor verify and restore from a known-good copy if needed")),
            Ok(report) if report.is_ok() => check("integrity", "ok", format!("verified {} pages; {} reachable", report.pages_checked, report.reachable_pages), None),
            Ok(report) => {
                let first = report.errors().next().map(|i| i.message.clone()).unwrap_or_else(|| "integrity verification found errors".to_string());
                check("integrity", "fail", first, Some("restore from a known-good copy; do not write to this database"))
            }
        }
    }
}

fn lock_check(path: &Path) -> DoctorCheck {
    match crate::serve_registry::read_info(path) {
        Ok(Some(info)) => {
            let listener = info.listeners.first().map(|l| format!("{}:{}", l.host, l.port)).unwrap_or_else(|| "unknown address".to_string());
            check("lock", "ok", format!("served by pgarbor pid {} on {listener}", info.pid), None)
        }
        _ => match open_container_readonly(path, VerifyLevel::None) {
            Ok(_) => check("lock", "ok", "database is not locked by another process", None),
            Err(e) if e.exit == Exit::LOCKED => check("lock", "warn", "locked by another process (not a pgarbor serve we can see)", Some("stop the process holding the database lock, or use pgarbor branch exec")),
            Err(e) => check("lock", "skip", format!("lock state unavailable: {}", e.message), Some("resolve file access errors before checking the lock")),
        }
    }
}

fn listener_check(path: &Path) -> DoctorCheck {
    let info = match crate::serve_registry::read_info(path) {
        Ok(Some(i)) => i,
        _ => return check("listener", "skip", "no live serve registry", Some("start pgarbor serve to check listener connectivity")),
    };
    for listener in &info.listeners {
        let addr = format!("{}:{}", listener.host, listener.port);
        let resolved = addr.to_socket_addrs().ok().and_then(|mut a| a.next());
        let connected = resolved.and_then(|a: SocketAddr| TcpStream::connect_timeout(&a, Duration::from_millis(500)).ok());
        if connected.is_none() {
            return check("listener", "fail", format!("listener {} does not accept connections", addr), Some("check whether the server is running and the listener address is reachable"));
        }
    }
    check("listener", "ok", format!("{} listener(s) accept connections", info.listeners.len()), None)
}

fn environment_check() -> DoctorCheck {
    let mut messages = Vec::new();
    let mut warn = false;
    if let Some(db) = std::env::var_os("PGARBOR_DB") {
        let p = PathBuf::from(db);
        if !p.is_file() { messages.push("PGARBOR_DB points to a missing file".to_string()); warn = true; }
    }
    if std::env::var_os("DATABASE_URL").is_some() { messages.push("DATABASE_URL is set; branch exec will refuse without --force-env".to_string()); }
    if messages.is_empty() { check("environment", "ok", "no relevant environment issues", None) }
    else { check("environment", if warn { "warn" } else { "ok" }, messages.join("; "), if warn { Some("correct or unset PGARBOR_DB") } else { None }) }
}

fn filesystem_check() -> DoctorCheck {
    check("filesystem", "skip", "reflink probe unavailable", Some("reflink availability is not exposed as a non-mutating probe"))
}

pub fn run(ctx: &mut Ctx, args: &FileArgs) -> CliResult<Exit> {
    let path = args.file.as_ref().or(args.db.db.as_ref()).map(|p| p.as_path());
    let mut checks = Vec::new();
    let mut corrupt = false;
    if let Some(path) = path {
        checks.push(file_check(path));
        if path.is_file() {
            checks.push(directory_check(path));
            checks.push(leftovers_check(path));
            let sb = superblock_check(path); corrupt |= sb.status == "fail"; checks.push(sb);
            let integrity = integrity_check(path); corrupt |= integrity.status == "fail"; checks.push(integrity);
            checks.push(lock_check(path));
            checks.push(listener_check(path));
        } else {
            for name in ["directory", "leftovers", "superblock", "integrity", "lock", "listener"] {
                checks.push(check(name, "skip", "database path is not an existing regular file", Some("provide an existing database file")));
            }
        }
        checks.push(environment_check());
        checks.push(filesystem_check());
    } else {
        checks.push(check("file", "fail", "no database path specified", Some("provide FILE or set --db / PGARBOR_DB")));
        checks.push(environment_check());
    }
    let lock_only = checks.iter().any(|c| c.name == "lock" && c.status == "warn")
        && !checks.iter().any(|c| c.status == "fail");
    let ok = !checks.iter().any(|c| c.status == "fail") && !lock_only;
    let exit = if corrupt { Exit::CORRUPT } else if lock_only { Exit::LOCKED }
        else if ok { Exit::OK } else { Exit::FAILURE };
    let output = DoctorOutput { path: path.map(|p| p.display().to_string()), ok, checks };
    if ctx.json { ctx.emit_json(&output)?; }
    else {
        for c in &output.checks {
            let label = match c.status.as_str() { "ok" => "ok", "warn" => "warn", "fail" => "FAIL", _ => "skip" };
            ctx.out_line(&format!("[{label}] {}: {}", c.name, c.message));
            if let Some(hint) = &c.hint { ctx.out_line(&format!("  hint: {hint}")); }
        }
    }
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("pgarbor-doctor-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
        fs::create_dir(&path).expect("create isolated test directory");
        path
    }

    #[test]
    fn leftover_compact_file_is_warning() {
        let dir = temp_dir();
        let db = dir.join("sample.pga");
        fs::write(dir.join("sample.pga.compact-tmp"), b"interrupted").expect("write fixture");
        let result = leftovers_check(&db);
        assert_eq!(result.status, "warn");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn garbage_file_is_reported_as_corrupt() {
        let dir = temp_dir();
        let db = dir.join("garbage.pga");
        fs::write(&db, b"not a PGArbor file").expect("write fixture");
        let result = superblock_check(&db);
        assert_eq!(result.status, "fail");
        let _ = fs::remove_dir_all(dir);
    }
}
