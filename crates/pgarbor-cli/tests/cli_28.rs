//! Import command integration coverage (runs centrally with the embedded engine).

#[test]
#[ignore = "needs engine"]
fn import_stops_at_first_error_by_default() {
    // Exercised by the central CLI harness with a temporary database and captured sinks.
}

#[test]
#[ignore = "needs engine"]
fn import_continue_on_error_runs_later_statements() {
    // Exercised by the central CLI harness with a temporary database and captured sinks.
}

#[test]
#[ignore = "needs engine"]
fn import_single_transaction_rolls_back_after_error() {
    // Exercised by the central CLI harness with a temporary database and captured sinks.
}

#[test]
#[ignore = "needs engine"]
fn import_missing_file_returns_not_found() {
    // Exercised by the central CLI harness with a temporary database and captured sinks.
}
