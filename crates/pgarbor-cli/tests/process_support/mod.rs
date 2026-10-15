#![allow(dead_code)]
use std::{io::Write, path::{Path, PathBuf}, process::{Command, Output, Stdio}};
use serde_json::Value;

pub struct Cli { pub dir: tempfile::TempDir, pub db: PathBuf }
impl Cli {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("test.pga");
        Self { dir, db }
    }
    pub fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pgarbor"));
        c.current_dir(self.dir.path()).env_remove("PGARBOR_DB").env_remove("PGARBOR_BRANCH")
            .env_remove("DATABASE_URL").stdin(Stdio::null());
        c
    }
    pub fn run(&self, args: &[&str]) -> Output { self.command().args(args).output().unwrap() }
    pub fn input(&self, args: &[&str], input: &str) -> Output {
        let mut child = self.command().args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }
    pub fn init(&self) -> Value { json_ok(self.run(&["--json", "init", path(&self.db)])) }
    pub fn sql(&self, branch: &str, sql: &str) -> Output {
        self.run(&["--json", "exec", path(&self.db), "--branch", branch, "-c", sql])
    }
    pub fn rows(&self, branch: &str, sql: &str) -> Value {
        let report = json_ok(self.sql(branch, sql));
        assert_eq!(report["ok"], true, "{report}");
        report["statements"].as_array().unwrap().last().unwrap()["rows"].clone()
    }
}
pub fn path(p: &Path) -> &str { p.to_str().unwrap() }
pub fn code(output: &Output, expected: i32) {
    assert_eq!(output.status.code(), Some(expected), "stdout={}\nstderr={}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
pub fn ok(output: Output) -> Output { code(&output, 0); output }
pub fn json_ok(output: Output) -> Value {
    let output = ok(output);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("invalid success JSON: {e}; stdout={}", String::from_utf8_lossy(&output.stdout)))
}
pub fn sqlstate(output: Output, state: &str) {
    code(&output,1);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let errors: Vec<_> = report["statements"].as_array().unwrap().iter().filter_map(|s|s["error"]["sqlstate"].as_str()).collect();
    assert_eq!(errors, [state], "{report}");
}
