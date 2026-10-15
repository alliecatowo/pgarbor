//! The `pgarbor` binary: a thin shell over the library (`pgarbor_cli::run_from_env`), so integration
//! tests can drive the same code. The default panic hook is kept on purpose (harness-02: the
//! standard `panicked at` line on stderr is how crashes are classified).
fn main() -> std::process::ExitCode {
    pgarbor_cli::run_from_env()
}
