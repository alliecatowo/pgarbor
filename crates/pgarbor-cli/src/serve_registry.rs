//! Discovery file of a serving process (lease CLI-23). Purpose: the `.pga` is exclusively locked by
//! the process that opened it (ADR-004), so other `pgarbor` invocations (`status`, `branch exec`,
//! lock-error diagnostics) need a way to find the owner and talk to it.
//!
//! ## Files (next to the database)
//! * `<db>.serve.json` - [`crate::reports::ServeInfo`] as pretty JSON, `format = "pgarbor-serve/1"`.
//!   Written atomically (write `<db>.serve.json.tmp`, `fsync`, `rename`) by `serve` after the listener
//!   is up; rewritten whenever listeners change (on-demand listeners); removed on clean shutdown.
//! * `<db>.serve.sock` - control socket (unix only, see `control.rs`). If the path would exceed
//!   100 bytes use `<temp_dir>/pgarbor-<16 hex of DefaultHasher(db absolute path)>.sock`.
//!
//! ## Liveness (stale files are normal after SIGKILL)
//! [`read_info`] returns `Some` only if: the file parses, `format` matches, `pid` is alive
//! (`signals::send_signal(pid, 0)`; EPERM counts as not alive: same-user assumption, documented), and
//! `db_path` canonicalizes to the same file as `db`. Otherwise it returns `None`; a stale file
//! (dead pid) is best-effort removed.
use std::path::{Path, PathBuf};

use crate::ctx::CliError;
use crate::reports::ServeInfo;

pub const INFO_FORMAT: &str = "pgarbor-serve/1";

/// `<db>.serve.json`.
pub fn info_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(".serve.json");
    PathBuf::from(s)
}

/// Control socket path (module docs).
pub fn control_socket_path(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(".serve.sock");
    let direct = PathBuf::from(s);
    if direct.as_os_str().len() <= 100 {
        return direct;
    }
    use std::hash::{Hash, Hasher};
    let abs = std::fs::canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    abs.hash(&mut h);
    std::env::temp_dir().join(format!("pgarbor-{:016x}.sock", h.finish()))
}

/// Atomically write the info file (module docs).
pub fn write_info(db: &Path, info: &ServeInfo) -> Result<(), CliError> {
    let path = info_path(db);
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let bytes = serde_json::to_vec_pretty(info)
        .map_err(|e| CliError::internal(format!("could not serialize serve info: {e}")))?;
    let mut file = std::fs::File::create(&tmp)
        .map_err(|e| CliError::io("could not create serve info temporary file", &e))?;
    use std::io::Write;
    file.write_all(&bytes).map_err(|e| CliError::io("could not write serve info", &e))?;
    file.sync_all().map_err(|e| CliError::io("could not sync serve info", &e))?;
    drop(file);
    std::fs::rename(&tmp, &path).map_err(|e| CliError::io("could not replace serve info", &e))
}

/// Read and validate (liveness, identity) the info file; `None` when absent or stale.
pub fn read_info(db: &Path) -> Result<Option<ServeInfo>, CliError> {
    let path = info_path(db);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CliError::io("could not read serve info", &e)),
    };
    let info: ServeInfo = match serde_json::from_slice(&bytes) {
        Ok(info) => info,
        Err(_) => return Ok(None),
    };
    if info.format != INFO_FORMAT {
        return Ok(None);
    }
    if !process_alive(info.pid) {
        // Stale records are expected after an unclean process exit. Cleanup is best effort.
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    let requested = match std::fs::canonicalize(db) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let recorded = match std::fs::canonicalize(&info.db_path) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    if requested != recorded {
        return Ok(None);
    }
    Ok(Some(info))
}

/// Remove the info file and the control socket file (best effort, ignores `NotFound`).
pub fn remove_info(db: &Path) -> Result<(), CliError> {
    for path in [info_path(db), control_socket_path(db)] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CliError::io("could not remove serve registry file", &e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reports::{ControlEndpoint, ListenerInfo};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        std::env::temp_dir().join(format!("pgarbor-serve-registry-{label}-{}-{stamp}.pga", std::process::id()))
    }

    fn sample(path: &Path) -> ServeInfo {
        ServeInfo {
            format: INFO_FORMAT.to_owned(),
            pid: std::process::id(),
            db_path: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned(),
            db_uuid: "test-db".to_owned(),
            started_unix_ms: 1,
            listeners: vec![ListenerInfo { branch: "main".into(), host: "127.0.0.1".into(), port: 5432, socket_dir: None, on_demand: false }],
            control: Some(ControlEndpoint { unix_path: "/tmp/test.sock".into() }),
        }
    }

    #[test]
    fn info_round_trip() {
        let db = temp_db("roundtrip");
        std::fs::write(&db, b"").expect("create test database path");
        let info = sample(&db);
        write_info(&db, &info).expect("write serve info");
        assert_eq!(read_info(&db).expect("read serve info"), Some(info));
        remove_info(&db).expect("remove serve info");
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn dead_pid_is_removed() {
        let db = temp_db("stale");
        std::fs::write(&db, b"").expect("create test database path");
        let mut info = sample(&db);
        info.pid = u32::MAX - 1;
        write_info(&db, &info).expect("write stale serve info");
        assert_eq!(read_info(&db).expect("read stale serve info"), None);
        assert!(!info_path(&db).exists());
        let _ = std::fs::remove_file(db);
    }

    #[test]
    fn wrong_format_is_absent() {
        let db = temp_db("format");
        std::fs::write(&db, b"").expect("create test database path");
        let mut info = sample(&db);
        info.format = "wrong".into();
        write_info(&db, &info).expect("write serve info");
        assert_eq!(read_info(&db).expect("read serve info"), None);
        remove_info(&db).expect("remove serve info");
        let _ = std::fs::remove_file(db);
    }
}

/// Is `pid` a live process (`kill(pid, 0)`)?
pub fn process_alive(pid: u32) -> bool {
    crate::signals::send_signal(pid, 0)
}
