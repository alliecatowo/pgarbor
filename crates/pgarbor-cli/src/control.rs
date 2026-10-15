//! Control socket of a serving process (lease CLI-24). Lets `pgarbor branch exec` obtain a listener
//! for ANY branch of a database that a long-running `pgarbor serve` has locked, and tells diagnostics
//! what is being served. Unix only (`#[cfg(unix)]`; on other targets `start` returns `0A000`).
//!
//! The socket uses newline-delimited JSON. A successful `listen` connection remains open for the
//! lifetime of the lease; closing it releases the listener reference.
use pgarbor_common::PgError;
use std::io::Read;
use std::path::PathBuf;

use pgarbor::Database;
use pgarbor_wire::ServerConfig;

use crate::ctx::CliError;

/// Everything the control server needs to start on-demand listeners that match the primary.
// CONTRACT
#[derive(Clone, Debug)]
pub struct ControlConfig {
    pub db_path: PathBuf,
    /// Branch served by the primary listener.
    pub primary_branch: String,
    pub primary_host: String,
    pub primary_port: u16,
    /// Template for on-demand listeners (auth policy, limits, default GUCs); `branch`/`listen` are
    /// overwritten per request.
    pub template: ServerConfig,
}

/// The running control server (fields private to the implementation).
pub struct ControlServer {
    #[cfg(unix)]
    state: std::sync::Arc<UnixState>,
    #[cfg(unix)]
    accept_thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    #[cfg(unix)]
    info: std::sync::Arc<std::sync::Mutex<crate::reports::ServeInfo>>,
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(unix)]
    _private: (),
    #[cfg(not(unix))]
    pub(crate) _private: (),
}

#[cfg(unix)]
struct UnixState {
    stopped: std::sync::atomic::AtomicBool,
    db: Database,
    cfg: ControlConfig,
    entries: std::sync::Mutex<std::collections::HashMap<String, OnDemand>>,
    workers: std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>,
}

#[cfg(unix)]
struct OnDemand {
    server: pgarbor_wire::Server,
    leases: usize,
    host: String,
    port: u16,
}

#[cfg(unix)]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    op: String,
    #[serde(default)]
    branch: Option<String>,
}

#[cfg(unix)]
fn error_response(message: &str, exit: u8) -> serde_json::Value {
    serde_json::json!({"ok":false,"error":message,"exit":exit})
}

#[cfg(unix)]
fn parse_request(line: &str) -> Option<Request> {
    serde_json::from_str(line).ok()
}

#[cfg(unix)]
fn success_response(host: &str, port: u16, on_demand: bool) -> serde_json::Value {
    serde_json::json!({"ok":true,"host":host,"port":port,"on_demand":on_demand})
}

impl ControlServer {
    /// Bind the socket and start serving (module docs).
    pub fn start(db: Database, cfg: ControlConfig) -> Result<ControlServer, CliError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            use std::os::unix::net::UnixListener;
            let info = crate::serve_registry::read_info(&cfg.db_path)?
                .ok_or_else(|| CliError::failure("serve info is missing or stale"))?;
            let socket_path = crate::serve_registry::control_socket_path(&cfg.db_path);
            if let Some(parent) = socket_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| CliError::io("create control socket directory", &e))?;
            }
            let listener = match UnixListener::bind(&socket_path) {
                Ok(listener) => listener,
                Err(first) if first.kind() == std::io::ErrorKind::AddrInUse => {
                    if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
                        return Err(CliError::io("bind control socket", &first));
                    }
                    std::fs::remove_file(&socket_path).map_err(|e| CliError::io("remove stale control socket", &e))?;
                    UnixListener::bind(&socket_path).map_err(|e| CliError::io("bind control socket", &e))?
                }
                Err(e) => return Err(CliError::io("bind control socket", &e)),
            };
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| CliError::io("set control socket permissions", &e))?;
            listener.set_nonblocking(true).map_err(|e| CliError::io("configure control socket", &e))?;
            let info = std::sync::Arc::new(std::sync::Mutex::new(info));
            let state = std::sync::Arc::new(UnixState {
                stopped: std::sync::atomic::AtomicBool::new(false), db, cfg,
                entries: std::sync::Mutex::new(std::collections::HashMap::new()),
                workers: std::sync::Mutex::new(Vec::new()),
            });
            let thread_state = std::sync::Arc::clone(&state);
            let thread_info = std::sync::Arc::clone(&info);
            let path_for_thread = socket_path.clone();
            let accept_thread = std::thread::Builder::new().name("pgarbor-control".into()).spawn(move || {
                while !thread_state.stopped.load(std::sync::atomic::Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let st = std::sync::Arc::clone(&thread_state);
                            let inf = std::sync::Arc::clone(&thread_info);
                            if let Ok(worker) = std::thread::Builder::new().name("pgarbor-control-client".into()).spawn(move || serve_connection(stream, st, inf)) {
                                if let Ok(mut workers) = thread_state.workers.lock() { workers.push(worker); }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(50)),
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
                    }
                }
                let _ = std::fs::remove_file(path_for_thread);
            }).map_err(|e| CliError::io("start control socket thread", &e))?;
            Ok(ControlServer { state, accept_thread: std::sync::Mutex::new(Some(accept_thread)), info, socket_path, _private: () })
        }
        #[cfg(not(unix))]
        {
            let _ = (db, cfg);
            Err(CliError::from(PgError::unimplemented("CLI-24", "Unix control sockets are unavailable")))
        }
    }

    /// Stop everything started by this control server (module docs). Idempotent.
    pub fn stop(&self) -> Result<(), CliError> {
        #[cfg(unix)]
        {
            self.state.stopped.store(true, std::sync::atomic::Ordering::Release);
            if let Ok(mut slot) = self.accept_thread.lock() {
                if let Some(thread) = slot.take() { let _ = thread.join(); }
            }
            if let Ok(mut entries) = self.state.entries.lock() {
                for (_, entry) in entries.drain() { let _ = entry.server.shutdown(); }
            }
            if let Ok(mut workers) = self.state.workers.lock() {
                for worker in workers.drain(..) { let _ = worker.join(); }
            }
            let _ = std::fs::remove_file(&self.socket_path);
            let mut info = self.info.lock().map_err(|_| CliError::internal("serve info lock poisoned"))?;
            info.listeners = self.listeners();
            crate::serve_registry::write_info(&self.state.cfg.db_path, &info)?;
            Ok(())
        }
        #[cfg(not(unix))]
        { Ok(()) }
    }

    /// Current `ServeInfo.listeners` (primary + on-demand) for rewriting the info file.
    pub fn listeners(&self) -> Vec<crate::reports::ListenerInfo> {
        #[cfg(unix)]
        {
            let mut out = vec![crate::reports::ListenerInfo { branch: self.state.cfg.primary_branch.clone(), host: self.state.cfg.primary_host.clone(), port: self.state.cfg.primary_port, socket_dir: None, on_demand: false }];
            if let Ok(entries) = self.state.entries.lock() {
                out.extend(entries.iter().map(|(branch, e)| crate::reports::ListenerInfo { branch: branch.clone(), host: e.host.clone(), port: e.port, socket_dir: None, on_demand: true }));
            }
            out
        }
        #[cfg(not(unix))]
        { Vec::new() }
    }
}

#[cfg(unix)]
impl Drop for ControlServer {
    fn drop(&mut self) { let _ = self.stop(); }
}

#[cfg(unix)]
fn rewrite_info(info: &std::sync::Arc<std::sync::Mutex<crate::reports::ServeInfo>>, state: &UnixState) {
    if let Ok(mut record) = info.lock() {
        let mut listeners = vec![crate::reports::ListenerInfo { branch: state.cfg.primary_branch.clone(), host: state.cfg.primary_host.clone(), port: state.cfg.primary_port, socket_dir: None, on_demand: false }];
        if let Ok(entries) = state.entries.lock() {
            listeners.extend(entries.iter().map(|(branch, e)| crate::reports::ListenerInfo { branch: branch.clone(), host: e.host.clone(), port: e.port, socket_dir: None, on_demand: true }));
        }
        record.listeners = listeners;
        let _ = crate::serve_registry::write_info(&state.cfg.db_path, &record);
    }
}

#[cfg(unix)]
fn serve_connection(mut stream: std::os::unix::net::UnixStream, state: std::sync::Arc<UnixState>, info: std::sync::Arc<std::sync::Mutex<crate::reports::ServeInfo>>) {
    use std::io::{BufRead, BufReader, Write};
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(100)));
    let mut reader = match stream.try_clone() { Ok(s) => BufReader::new(s), Err(_) => return };
    let mut bytes = Vec::new();
    if (&mut reader).take(65_537).read_until(b'\n', &mut bytes).is_err() || bytes.len() > 65_536 || bytes.last() != Some(&b'\n') { return; }
    let line = match String::from_utf8(bytes) { Ok(s) => s, Err(_) => { let _ = writeln!(stream, "{}", error_response("bad request", 1)); return; } };
    let req = match parse_request(line.trim_end_matches(['\r', '\n'])) { Some(r) => r, None => { let _ = writeln!(stream, "{}", error_response("bad request", 1)); return; } };
    let mut lease_branch: Option<String> = None;
    let response = match req.op.as_str() {
        "ping" => serde_json::json!({"ok":true,"pid":std::process::id()}),
        "status" => match info.lock() { Ok(i) => serde_json::json!({"ok":true,"info":&*i}), Err(_) => error_response("control server unavailable", 1) },
        "listen" => {
            let branch = match req.branch { Some(b) if !b.is_empty() => b, _ => { let _ = writeln!(stream, "{}", error_response("bad request", 1)); return; } };
            if branch == state.cfg.primary_branch {
                let _ = writeln!(stream, "{}", success_response(&state.cfg.primary_host, state.cfg.primary_port, false));
                return;
            }
            let mut entries = match state.entries.lock() { Ok(v) => v, Err(_) => { let _ = writeln!(stream, "{}", error_response("control server unavailable", 1)); return; } };
            if let Some(entry) = entries.get_mut(&branch) {
                entry.leases += 1;
                lease_branch = Some(branch.clone());
                success_response(&entry.host, entry.port, true)
            } else {
                // Session creation validates that the requested branch exists and is active.
                if state.db.session(&branch, pgarbor::SessionOptions::default()).is_err() {
                    let _ = writeln!(stream, "{}", error_response(&format!("branch \"{branch}\" does not exist"), 5));
                    return;
                }
                let mut config = state.cfg.template.clone();
                config.branch = branch.clone();
                config.listen = vec![pgarbor_wire::ListenAddr::Tcp { host: "127.0.0.1".into(), port: 0 }];
                let mut config = config;
                config.allow_nonloopback = false;
                match pgarbor_wire::Server::start(state.db.clone(), config) {
                    Ok(server) => match server.tcp_addr() {
                        Some(addr) => {
                            let host = "127.0.0.1".to_string();
                            let port = addr.port();
                            lease_branch = Some(branch.clone());
                            entries.insert(branch, OnDemand { server, leases: 1, host: host.clone(), port });
                            drop(entries);
                            rewrite_info(&info, &state);
                            success_response(&host, port, true)
                        }
                        None => error_response("listener did not bind TCP", 1),
                    },
                    Err(e) => error_response(&e.message, e.sqlstate.code().eq("3D000").then_some(5).unwrap_or(1)),
                }
            }
        }
        _ => { let _ = writeln!(stream, "{}", error_response("bad request", 1)); return; }
    };
    let is_lease = lease_branch.is_some() && response.get("on_demand").and_then(serde_json::Value::as_bool) == Some(true) && response.get("ok").and_then(serde_json::Value::as_bool) == Some(true);
    let _ = writeln!(stream, "{response}");
    if !is_lease { return; }
    // Remain connected as the reference-count lease; poll so stop() can join this worker.
    let mut byte = [0u8; 1];
    loop {
        if state.stopped.load(std::sync::atomic::Ordering::Acquire) { break; }
        match reader.read(&mut byte) { Ok(0) => break, Ok(_) => {}, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::Interrupted => {}, Err(_) => break }
    }
    if let (Some(branch), Ok(mut entries)) = (lease_branch, state.entries.lock()) {
        let stop_server = if let Some(entry) = entries.get_mut(&branch) {
            entry.leases = entry.leases.saturating_sub(1);
            if entry.leases == 0 { entries.remove(&branch) } else { None }
        } else { None };
        drop(entries);
        if let Some(entry) = stop_server { let _ = entry.server.shutdown(); }
        rewrite_info(&info, &state);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn request_parser_accepts_known_shapes_and_rejects_bad_json() {
        assert_eq!(parse_request(r#"{"op":"ping"}"#).map(|r| r.op), Some("ping".into()));
        assert_eq!(parse_request(r#"{"op":"listen","branch":"feature"}"#).and_then(|r| r.branch), Some("feature".into()));
        assert!(parse_request("not json").is_none());
        assert!(parse_request(r#"{"op":"ping","extra":true}"#).is_none());
    }

    #[test]
    fn response_builders_have_protocol_fields() {
        assert_eq!(success_response("127.0.0.1", 5432, true), serde_json::json!({"ok":true,"host":"127.0.0.1","port":5432,"on_demand":true}));
        assert_eq!(error_response("missing", 5), serde_json::json!({"ok":false,"error":"missing","exit":5}));
    }
}
