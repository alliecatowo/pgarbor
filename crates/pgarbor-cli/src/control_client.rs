//! Client side of the control socket (lease CLI-25), used by `branch exec` when the database is
//! locked by a running `pgarbor serve`.
//!
//! [`request_listener`] connects to `info.control.unix_path`, sends
//! `{"op":"listen","branch":"<branch>"}\n`, reads one response line (timeout 10 s) and returns a
//! [`ListenLease`] that owns the open connection. Dropping the lease closes the connection, which
//! releases the on-demand listener on the server side (protocol in `control.rs`).
//!
//! Errors: no control endpoint in `info` (older serve / `--no-registry`): exit 4 `database file
//! "<db>" is in use by pgarbor serve (pid N) which does not accept control connections`; connect
//! failure: exit 4 with the OS error; `{"ok":false,...}` response: `CliError::new(Exit(exit), error)`
//! (unknown branch -> exit 5); malformed response: exit 1 `invalid response from serve control socket`.
//! Non-unix targets: exit 8 unsupported.
use crate::ctx::CliError;
use crate::exit::Exit;
use crate::reports::ServeInfo;

/// A granted listener; the TCP endpoint to put into the URL.
pub struct ListenLease {
    pub host: String,
    pub port: u16,
    pub branch: String,
    /// The server had to start a new listener for this branch (false = reused the primary).
    pub on_demand: bool,
    /// Keeps the lease alive (a `UnixStream` on unix).
    pub(crate) _conn: Box<dyn std::any::Any>,
}

/// Ask the serving process for a listener on `branch` (module docs).
pub fn request_listener(info: &ServeInfo, branch: &str) -> Result<ListenLease, CliError> {
    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let endpoint = info.control.as_ref().ok_or_else(|| {
            CliError::new(
                Exit::LOCKED,
                format!(
                    "database file \"{}\" is in use by pgarbor serve (pid {}) which does not accept control connections",
                    info.db_path, info.pid
                ),
            )
        })?;
        let mut stream = UnixStream::connect(&endpoint.unix_path).map_err(|error| {
            CliError::new(
                Exit::LOCKED,
                format!("could not connect to serve control socket: {error}"),
            )
        })?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| CliError::new(Exit::FAILURE, format!("could not set control socket timeout: {error}")))?;

        let request = serde_json::json!({"op": "listen", "branch": branch});
        writeln!(stream, "{request}")
            .map_err(|error| CliError::new(Exit::FAILURE, format!("could not write serve control request: {error}")))?;

        let mut response_line = String::new();
        let read_result = {
            let mut reader = BufReader::new(&mut stream);
            reader.read_line(&mut response_line)
        };
        read_result.map_err(|_| invalid_response())?;
        parse_listener_response(&response_line).map(|(host, port, on_demand)| ListenLease {
            host,
            port,
            branch: branch.to_owned(),
            on_demand,
            _conn: Box::new(stream),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (info, branch);
        Err(CliError::new(Exit::UNSUPPORTED, "control sockets are not supported on this platform"))
    }
}

fn invalid_response() -> CliError {
    CliError::new(Exit::FAILURE, "invalid response from serve control socket")
}

fn parse_listener_response(line: &str) -> Result<(String, u16, bool), CliError> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| invalid_response())?;
    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
        let message = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid_response)?;
        let exit = value
            .get("exit")
            .and_then(serde_json::Value::as_u64)
            .and_then(|code| u8::try_from(code).ok())
            .map(Exit)
            .unwrap_or(Exit::FAILURE);
        return Err(CliError::new(exit, message));
    }
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(invalid_response());
    }
    let host = value
        .get("host")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(invalid_response)?
        .to_owned();
    let port = value
        .get("port")
        .and_then(serde_json::Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .ok_or_else(invalid_response)?;
    let on_demand = value
        .get("on_demand")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(invalid_response)?;
    Ok((host, port, on_demand))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_response() {
        let parsed = parse_listener_response(
            r#"{"ok":true,"host":"127.0.0.1","port":41873,"on_demand":true}"#,
        );
        assert!(matches!(parsed, Ok((host, 41873, true)) if host == "127.0.0.1"));
    }

    #[test]
    fn parses_error_response_and_exit() {
        let parsed = parse_listener_response(r#"{"ok":false,"error":"unknown branch","exit":5}"#);
        assert!(matches!(parsed, Err(error) if error.exit == Exit::NOT_FOUND && error.message == "unknown branch"));
    }

    #[test]
    fn rejects_malformed_success_response() {
        let parsed = parse_listener_response(r#"{"ok":true,"host":"localhost"}"#);
        assert_eq!(parsed.err().map(|error| error.message), Some("invalid response from serve control socket".to_owned()));
    }
}
