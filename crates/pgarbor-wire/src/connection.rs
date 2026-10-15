//! One client connection from accept to close (lease WIR-17). PG: `postmaster/postmaster.c`
//! `BackendInitialize`, `tcop/postgres.c` `PostgresMain` (startup half), `utils/init/postinit.c`.
//!
//! [`serve_connection`] is the panic boundary for one accepted socket. Setup and protocol work are
//! delegated to the startup, authentication, and session-loop modules.
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Instant;

use pgarbor::{SessionOptions, TxnStatus};
use pgarbor_common::{PgError, Severity, SqlState};

use crate::backend::Backend;
use crate::conn_io::ConnIo;
use crate::error::{WireError, WireResult};
use crate::limits::ConnGuard;
use crate::shared::Shared;
use crate::transport::Stream;

/// Run one connection to completion. `_threads_permit` is released on every return or unwind.
pub fn serve_connection(shared: Arc<Shared>, stream: Stream, _threads_permit: ConnGuard) {
    let conn_id = shared.next_conn_id();
    let result = catch_unwind(AssertUnwindSafe(|| run_connection_inner(&shared, stream, conn_id)));
    if let Err(payload) = result {
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&'static str>().copied())
            .unwrap_or("non-string panic payload");
        eprintln!("pgarbor-wire: connection {conn_id} panicked: {message}");
    }
}

fn send_fatal(io: &mut ConnIo, error: PgError) {
    let fatal = error.with_severity(Severity::Fatal);
    let _ = io.send(&crate::error_fields::error_message(&fatal));
    let _ = io.flush();
}

fn fatal_state(state: SqlState, message: impl Into<String>) -> PgError {
    PgError::new(state, message).with_severity(Severity::Fatal)
}

/// The fallible body of [`serve_connection`] (steps 1-10).
pub fn run_connection(shared: &Arc<Shared>, stream: Stream) -> WireResult<()> {
    let conn_id = shared.next_conn_id();
    run_connection_inner(shared, stream, conn_id)
}

fn run_connection_inner(shared: &Arc<Shared>, stream: Stream, conn_id: u64) -> WireResult<()> {
    let socket = match stream.try_clone() {
        Ok(socket) => socket,
        Err(error) => return Err(WireError::Io(error)),
    };
    let _registry_guard = shared.registry.register(conn_id, socket);
    let mut io = ConnIo::new(stream, conn_id, &shared.config)?;
    let deadline = Instant::now() + shared.config.startup_timeout;
    let startup = match crate::startup_flow::negotiate(&mut io, shared, deadline) {
        Ok(crate::startup_flow::Outcome::Proceed(info)) => info,
        Ok(crate::startup_flow::Outcome::CancelHandled | crate::startup_flow::Outcome::Dropped) => {
            io.shutdown();
            return Ok(());
        }
        Err(error) => {
            if let WireError::Pg(pg) = &error { send_fatal(&mut io, pg.clone()); }
            io.shutdown();
            return Err(error);
        }
    };

    if shared.is_stopping() {
        let error = fatal_state(SqlState([b'5', b'7', b'P', b'0', b'3']), "the database system is shutting down");
        send_fatal(&mut io, error);
        io.shutdown();
        return Ok(());
    }
    let _session_permit = match shared.sessions.try_acquire() {
        Some(permit) => permit,
        None => {
            send_fatal(&mut io, fatal_state(SqlState([b'5', b'3', b'3', b'0', b'0']), "sorry, too many clients already"));
            io.shutdown();
            return Ok(());
        }
    };

    if let Err(error) = crate::auth::authenticate(&mut io, shared, &startup, deadline) {
        if let WireError::Pg(pg) = &error { send_fatal(&mut io, pg.clone()); }
        io.shutdown();
        return Err(error);
    }

    let peer = io.peer().clone();
    let options = SessionOptions {
        user: startup.user,
        database: startup.database,
        application_name: startup.application_name,
        client_addr: peer.client_addr(),
        client_port: peer.client_port(),
        cancel_key_len: startup.cancel_key_len,
        startup_gucs: startup.startup_gucs,
    };
    let mut session = match shared.db.connect(Some(&shared.config.branch), options.clone()) {
        Ok(session) => session,
        Err(error) => {
            send_fatal(&mut io, error);
            io.shutdown();
            return Ok(());
        }
    };

    let initial_params = match session.startup_param_status() {
        Ok(params) => params,
        Err(error) => {
            send_fatal(&mut io, error);
            session.close();
            io.shutdown();
            return Ok(());
        }
    };
    for (name, value) in &initial_params {
        if let Err(error) = io.send(&Backend::ParameterStatus { name: name.clone(), value: value.clone() }) {
            session.close();
            io.shutdown();
            return Err(error);
        }
    }
    let (pid, secret) = session.backend_key();
    let startup_messages = io.send(&Backend::BackendKeyData { pid, secret })
        .and_then(|_| io.send(&Backend::ReadyForQuery(TxnStatus::Idle)))
        .and_then(|_| io.flush());
    if let Err(error) = startup_messages {
        session.close();
        io.shutdown();
        return Err(error);
    }
    shared.registry.set_session(conn_id, session.cancel_token(), &options.user);

    let shared_io = std::rc::Rc::new(std::cell::RefCell::new(io));
    let _copy_handle = match crate::copy::install(&mut session, &shared_io) {
        Ok(handle) => Some(handle),
        Err(_) => None,
    };
    let mut ctx = crate::session_loop::SessionCtx::new(
        std::rc::Rc::clone(&shared_io), shared, &initial_params,
    );
    let outcome = catch_unwind(AssertUnwindSafe(|| crate::session_loop::run(&mut ctx, &mut session)));
    match outcome {
        Ok(Ok(())) | Ok(Err(WireError::Closed | WireError::Io(_))) => {}
        Ok(Err(WireError::Shutdown)) => {
            let error = fatal_state(SqlState([b'5', b'7', b'P', b'0', b'1']), "terminating connection due to administrator command");
            if let Ok(mut io) = shared_io.try_borrow_mut() { send_fatal(&mut io, error); }
        }
        Ok(Err(WireError::Pg(error))) => {
            if let Ok(mut io) = shared_io.try_borrow_mut() { send_fatal(&mut io, error); }
        }
        Err(payload) => {
            let message = payload.downcast_ref::<String>().map(String::as_str)
                .or_else(|| payload.downcast_ref::<&'static str>().copied()).unwrap_or("non-string panic payload");
            eprintln!("pgarbor-wire: connection {conn_id} panicked: {message}");
            let error = fatal_state(SqlState([b'X', b'X', b'0', b'0', b'0']), "internal error");
            if let Ok(mut io) = shared_io.try_borrow_mut() { send_fatal(&mut io, error); }
        }
    }
    session.close();
    if let Ok(mut io) = shared_io.try_borrow_mut() { io.shutdown(); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_state_preserves_state_and_sets_fatal_severity() {
        let error = fatal_state(SqlState([b'5', b'7', b'P', b'0', b'3']), "the database system is shutting down");
        assert_eq!(error.sqlstate.code(), "57P03");
        assert_eq!(error.severity, Severity::Fatal);
        assert_eq!(error.message, "the database system is shutting down");
    }
}

#[cfg(test)]
mod duplex_e2e {
    use super::*;
    use crate::buf::MsgReader;
    fn frame(bytes: &mut Vec<u8>, tag: u8, body: &[u8]) {
        bytes.push(tag);bytes.extend_from_slice(&((body.len()+4) as u32).to_be_bytes());bytes.extend_from_slice(body);
    }
    fn startup() -> Vec<u8> {
        let mut body=196608u32.to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0postgres\0database\0postgres\0\0");
        let mut data=((body.len()+4) as u32).to_be_bytes().to_vec();data.extend(body);data
    }
    fn query(bytes: &mut Vec<u8>, sql: &str) {let mut body=sql.as_bytes().to_vec();body.push(0);frame(bytes,b'Q',&body);}
    fn exchange(mut input: Vec<u8>) -> Vec<(u8,Vec<u8>)> {
        frame(&mut input,b'X',&[]);
        let db=pgarbor::Database::in_memory().expect("database");
        let shared=crate::shared::Shared::new(db,crate::ServerConfig::loopback("main"));
        let (stream,output)=crate::transport::scripted(input);
        run_connection(&shared,stream).expect("wire connection");
        let bytes=output.lock().expect("output").clone();let mut at=0;let mut frames=Vec::new();
        while at<bytes.len() {
            assert!(bytes.len()-at>=5);let tag=bytes[at];
            let n=u32::from_be_bytes(bytes[at+1..at+5].try_into().expect("length")) as usize;
            assert!(n>=4 && at+1+n<=bytes.len());frames.push((tag,bytes[at+5..at+1+n].to_vec()));at+=1+n;
        }
        frames
    }
    fn rows(frames: &[(u8,Vec<u8>)]) -> Vec<Vec<Option<String>>> {
        frames.iter().filter(|f|f.0==b'D').map(|(_,b)| {
            let mut r=MsgReader::new(b);let n=r.read_i16().expect("fields");assert!(n>=0);
            let row=(0..n).map(|_|{let n=r.read_i32().expect("length");if n==-1 {None} else {assert!(n>=0);Some(std::str::from_utf8(r.take(n as usize).expect("value")).expect("UTF8").to_owned())}}).collect();r.finish().expect("end");row
        }).collect()
    }
    fn no_errors(frames: &[(u8,Vec<u8>)]) {assert!(!frames.iter().any(|f|f.0==b'E'),"errors: {frames:?}");}
    #[test]
    fn fresh_show_begin_rollback_and_select_over_wire_session() {
        let mut input=startup();
        for sql in ["SHOW server_version","CREATE TABLE t(id int)","BEGIN","INSERT INTO t VALUES(1)","ROLLBACK","SELECT count(*) FROM t"] {query(&mut input,sql);}
        let out=exchange(input);no_errors(&out);
        assert_eq!(rows(&out),vec![vec![Some("18.6".into())],vec![Some("0".into())]]);
        assert!(out.iter().any(|(t,b)|*t==b'Z' && b==b"T"));
        assert_eq!(out.iter().filter(|f|f.0==b'Z').count(),7);
    }
    #[test]
    fn update_delete_and_information_schema_over_wire_session() {
        let mut input=startup();
        for sql in ["CREATE TABLE t(id int primary key,v text)","INSERT INTO t VALUES(1,'a'),(2,'b')","UPDATE t SET v=v || '!' WHERE id=1","DELETE FROM t WHERE id=2","SELECT v FROM t ORDER BY id","SELECT table_name FROM information_schema.tables WHERE table_schema='public'"] {query(&mut input,sql);}
        let out=exchange(input);no_errors(&out);
        assert_eq!(rows(&out),vec![vec![Some("a!".into())],vec![Some("t".into())]]);
        assert!(out.iter().any(|(t,b)|*t==b'C' && b==b"UPDATE 1\0"));
        assert!(out.iter().any(|(t,b)|*t==b'C' && b==b"DELETE 1\0"));
    }
    #[test]
    fn extended_text_parameter_uses_real_bind_execute_sync() {
        let mut input=startup();
        frame(&mut input,b'P',b"\0SELECT $1::int + 1\0\0\0");
        frame(&mut input,b'B',b"\0\0\0\0\0\x01\0\0\0\x0241\0\0");
        frame(&mut input,b'D',b"P\0");
        frame(&mut input,b'E',b"\0\0\0\0\0");
        frame(&mut input,b'S',b"");
        let out=exchange(input);no_errors(&out);
        assert_eq!(rows(&out),vec![vec![Some("42".into())]]);
        assert_eq!(out.iter().filter(|f|f.0==b'Z').count(),2);
        for tag in [b'1',b'2',b'T',b'D',b'C'] {assert!(out.iter().any(|f|f.0==tag));}
    }
}
