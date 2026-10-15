use std::io::Write;
use std::sync::{Arc, Mutex};

use pgarbor_cli::{ctx::Ctx, run_with};

#[derive(Clone)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "output lock poisoned"))?.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn run(args: Vec<String>, json: bool) -> (u8, String, String) {
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let out = stdout.clone();
    let err = stderr.clone();
    let code = run_with(args, move |_| Ctx::with_sinks(json, Box::new(Shared(out)), Box::new(Shared(err))));
    let out = String::from_utf8_lossy(&stdout.lock().map(|v| v.clone()).unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&stderr.lock().map(|v| v.clone()).unwrap_or_default()).into_owned();
    (code.code(), out, err)
}

#[test]
#[ignore = "needs engine"]
fn exec_renders_query_results() {
    let path = std::env::temp_dir().join(format!("pgarbor-cli-10-{}.pga", std::process::id()));
    let _ = std::fs::remove_file(&path);
    assert_eq!(run(vec!["pgarbor".into(), "init".into(), path.display().to_string()], false).0, 0);
    let (code, out, _) = run(vec!["pgarbor".into(), "exec".into(), path.display().to_string(), "-c".into(), "create table t(a int); insert into t values (1); select * from t".into()], false);
    assert_eq!(code, 0);
    assert!(out.contains("(1 row)"));
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore = "needs engine"]
fn exec_json_contains_statement_report() {
    let path = std::env::temp_dir().join(format!("pgarbor-cli-10-json-{}.pga", std::process::id()));
    let _ = std::fs::remove_file(&path);
    assert_eq!(run(vec!["pgarbor".into(), "init".into(), path.display().to_string()], false).0, 0);
    let (code, out, _) = run(vec!["pgarbor".into(), "--json".into(), "exec".into(), path.display().to_string(), "-c".into(), "select 1".into()], true);
    assert_eq!(code, 0);
    let report: serde_json::Value = serde_json::from_str(&out).expect("exec emits valid report JSON");
    assert!(report["ok"].as_bool().unwrap_or(false));
    assert_eq!(report["statements"].as_array().map(Vec::len), Some(1));
    let _ = std::fs::remove_file(path);
}

#[test]
#[ignore = "needs engine"]
fn exec_reports_sql_errors() {
    let path = std::env::temp_dir().join(format!("pgarbor-cli-10-error-{}.pga", std::process::id()));
    let _ = std::fs::remove_file(&path);
    assert_eq!(run(vec!["pgarbor".into(), "init".into(), path.display().to_string()], false).0, 0);
    let (code, _, err) = run(vec!["pgarbor".into(), "exec".into(), path.display().to_string(), "-c".into(), "select from".into()], false);
    assert_eq!(code, 1);
    assert!(err.contains("ERROR:"));
    let _ = std::fs::remove_file(path);
}
