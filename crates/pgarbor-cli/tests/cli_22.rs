#![cfg(unix)]

use std::ffi::OsString;

fn invoke(args: &[&str]) -> pgarbor_cli::exit::Exit {
    pgarbor_cli::run_with(args.iter().map(OsString::from), |cli| {
        pgarbor_cli::ctx::Ctx::with_sinks(cli.json, Box::new(Vec::<u8>::new()), Box::new(Vec::<u8>::new()))
    })
}

#[test]
#[ignore = "needs engine and wire"]
fn branch_exec_injects_url_and_propagates_child_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("exec.pga");
    let dbs = db.to_string_lossy().into_owned();
    assert_eq!(invoke(&["pgarbor", "init", &dbs]), pgarbor_cli::exit::Exit::OK);

    let saved = dir.path().join("url.txt");
    let saved_s = saved.to_string_lossy().into_owned();
    let script = format!("printf '%s' \"$DATABASE_URL\" > '{}'", saved_s.replace('\'', "'\\''"));
    assert_eq!(invoke(&["pgarbor", "branch", "exec", "main", "--db", &dbs, "--", "sh", "-c", &script]), pgarbor_cli::exit::Exit::OK);
    let url = std::fs::read_to_string(&saved).expect("child wrote DATABASE_URL");
    assert!(url.starts_with("postgres://postgres@127.0.0.1:"));
    assert!(url.ends_with("/postgres?sslmode=disable"));
    let port = url.split('@').nth(1).and_then(|s| s.split(':').nth(1)).and_then(|s| s.split('/').next())
        .and_then(|s| s.parse::<u16>().ok()).expect("valid listener URL");
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "owned listener should be closed after child exit");

    assert_eq!(invoke(&["pgarbor", "branch", "exec", "main", "--db", &dbs, "--", "sh", "-c", "exit 7"]).code(), 7);
    assert_eq!(invoke(&["pgarbor", "branch", "exec", "main", "--db", &dbs, "--", "sh", "-c", "kill -TERM $$"]).code(), 143);
    assert_eq!(invoke(&["pgarbor", "branch", "exec", "main", "--db", &dbs, "--", "pgarbor-cli-no-such-command"]).code(), 127);
}

#[test]
#[ignore = "needs engine and wire"]
fn existing_url_is_refused_unless_force_env_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("env.pga");
    let dbs = db.to_string_lossy().into_owned();
    assert_eq!(invoke(&["pgarbor", "init", &dbs]), pgarbor_cli::exit::Exit::OK);
    let key = "PGARBOR_CLI22_TEST_URL";
    // SAFETY: this test uses a unique variable name and no other test reads or writes it.
    unsafe { std::env::set_var(key, "already-set"); }
    let refused = invoke(&["pgarbor", "branch", "exec", "main", "--db", &dbs, "--env-name", key, "--", "sh", "-c", "exit 0"]);
    assert_eq!(refused, pgarbor_cli::exit::Exit::REFUSED);
    let forced = invoke(&["pgarbor", "branch", "exec", "main", "--db", &dbs, "--env-name", key, "--force-env", "--", "sh", "-c", "test \"$PGARBOR_CLI22_TEST_URL\" != already-set"]);
    // SAFETY: paired with the unique test-only assignment above.
    unsafe { std::env::remove_var(key); }
    assert_eq!(forced, pgarbor_cli::exit::Exit::OK);
}
