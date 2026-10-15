"""Unit tests for pgarbor_factory.build (lease FAC-08). Pure parsing and clustering only; no cargo."""

import json
from pathlib import Path

from pgarbor_factory.build import (
    BuildReport,
    Diagnostic,
    _parse_test_output,
    cluster_errors,
    parse_cargo_messages,
)

REPO = Path("/repo")

OWNERS = {
    "crates/pgarbor-core/src/storage/page.rs": "STO-01",
    "crates/pgarbor-wire/src/codec.rs": "WIRE-02",
    "crates/pgarbor-fault/src/a.rs": "FLT-01",
    "crates/pgarbor-fault/src/b.rs": "FLT-01",
}


def _span(file_name: str, line: int, primary: bool = True) -> dict:
    return {
        "file_name": file_name,
        "byte_start": 0,
        "byte_end": 1,
        "line_start": line,
        "line_end": line,
        "column_start": 5,
        "column_end": 9,
        "is_primary": primary,
        "label": None,
        "text": [],
    }


def _error(pkg: str, code: str, message: str, file_name: str, line: int) -> dict:
    return {
        "reason": "compiler-message",
        "package_id": f"path+file:///repo/crates/{pkg}#{pkg}@0.1.0",
        "manifest_path": f"/repo/crates/{pkg}/Cargo.toml",
        "target": {"kind": ["lib"], "name": pkg.replace("-", "_"), "src_path": "x"},
        "message": {
            "message": message,
            "code": {"code": code, "explanation": None},
            "level": "error",
            "spans": [_span(file_name, line)],
            "children": [],
            "rendered": f"error[{code}]: {message}\n",
        },
    }


def _canned_lines() -> list[str]:
    e308_core = _error("pgarbor-core", "E0308", "mismatched types",
                       "/repo/crates/pgarbor-core/src/storage/page.rs", 42)
    e308_wire = _error("pgarbor-wire", "E0308", "mismatched types",
                       "crates/pgarbor-wire/src/codec.rs", 7)
    e425_x = _error("pgarbor-fault", "E0425", "cannot find value `x` in this scope",
                    "crates/pgarbor-fault/src/a.rs", 3)
    e425_y = _error("pgarbor-fault", "E0425", "cannot find value `y` in this scope",
                    "crates/pgarbor-fault/src/b.rs", 9)
    warning = {
        "reason": "compiler-message",
        "package_id": "path+file:///repo/crates/pgarbor-core#pgarbor-core@0.1.0",
        "target": {"kind": ["lib"], "name": "pgarbor_core", "src_path": "x"},
        "message": {
            "message": "unused variable: `z`",
            "code": {"code": "unused_variables", "explanation": None},
            "level": "warning",
            "spans": [_span("crates/pgarbor-core/src/lib.rs", 12)],
            "children": [],
            "rendered": "warning: unused variable: `z`\n",
        },
    }
    return [
        "   Compiling pgarbor-core v0.1.0 (/repo/crates/pgarbor-core)",
        json.dumps({"reason": "compiler-artifact", "target": {"name": "pgarbor_core"}}),
        json.dumps(e308_core),
        json.dumps(e308_wire),
        json.dumps(e425_x),
        json.dumps(e425_y),
        json.dumps(warning),
        json.dumps({"reason": "build-finished", "success": False}),
    ]


def test_parse_collects_errors_and_warnings():
    errors, warnings, success = parse_cargo_messages(_canned_lines(), OWNERS, REPO)
    assert success is False
    assert len(errors) == 4
    assert warnings == {"pgarbor-core": 1}
    by_file = {e.file: e for e in errors}
    core = by_file["crates/pgarbor-core/src/storage/page.rs"]
    assert by_file["crates/pgarbor-wire/src/codec.rs"].lease_id == "WIRE-02"
    assert core.crate == "pgarbor-core"
    assert core.file == "crates/pgarbor-core/src/storage/page.rs"  # absolute path made repo-relative
    assert core.line == 42
    assert core.lease_id == "STO-01"
    assert core.rendered.startswith("error[E0308]")


def test_non_json_lines_and_warnings_do_not_become_errors():
    errors, warnings, _ = parse_cargo_messages(_canned_lines(), OWNERS, REPO)
    assert all(e.level.startswith("error") for e in errors)
    assert not any("unused" in e.message for e in errors)


def test_unleased_file_has_empty_lease():
    lines = [json.dumps(_error("pgarbor-core", "E0425", "cannot find value `q` in this scope",
                               "crates/pgarbor-core/src/lib.rs", 1))]
    errors, _, _ = parse_cargo_messages(lines, OWNERS, REPO)
    assert errors[0].lease_id == ""


def test_build_finished_success_true_sets_flag():
    lines = [json.dumps({"reason": "build-finished", "success": True})]
    errors, warnings, success = parse_cargo_messages(lines, OWNERS, REPO)
    assert errors == [] and warnings == {} and success is True


def test_missing_build_finished_is_not_success():
    errors, _, success = parse_cargo_messages([], OWNERS, REPO)
    assert errors == [] and success is False


def test_duplicate_targets_are_deduplicated():
    line = json.dumps(_error("pgarbor-fault", "E0425", "cannot find value `x` in this scope",
                             "crates/pgarbor-fault/src/a.rs", 3))
    errors, _, _ = parse_cargo_messages([line, line], OWNERS, REPO)
    assert len(errors) == 1


def test_internal_compiler_error_counts_as_error():
    obj = _error("pgarbor-core", "E0308", "mismatched types", "crates/pgarbor-core/src/lib.rs", 1)
    obj["message"]["level"] = "error: internal compiler error"
    errors, _, _ = parse_cargo_messages([json.dumps(obj)], OWNERS, REPO)
    assert len(errors) == 1 and errors[0].level == "error: internal compiler error"


def test_two_e0425_cluster_together_and_warnings_never_cluster():
    errors, _, _ = parse_cargo_messages(_canned_lines(), OWNERS, REPO)
    clusters = cluster_errors(errors)
    e425 = [c for c in clusters if c.code == "E0425"]
    assert len(e425) == 1
    cluster = e425[0]
    assert cluster.template == "cannot find value `?` in this scope"
    assert cluster.count == 2
    assert cluster.leases == ["FLT-01"]
    assert cluster.files == [
        "crates/pgarbor-fault/src/a.rs",
        "crates/pgarbor-fault/src/b.rs",
    ]
    assert cluster.root_hint == "local"
    assert len(cluster.examples) == 2
    assert all("unused" not in c.key for c in clusters)


def test_clusters_sorted_by_lease_count_then_count():
    errors, _, _ = parse_cargo_messages(_canned_lines(), OWNERS, REPO)
    clusters = cluster_errors(errors)
    assert [c.code for c in clusters] == ["E0308", "E0425"]
    assert clusters[0].leases == ["STO-01", "WIRE-02"]
    assert clusters[0].count == 2


def test_three_leases_with_external_path_is_interface_drift():
    owners = {
        "crates/a/src/x.rs": "A-01",
        "crates/b/src/y.rs": "B-01",
        "crates/c/src/z.rs": "C-01",
    }
    lines = [
        json.dumps(_error("pgarbor-wire", "E0308",
                          "mismatched types: expected `pgarbor_core::Page`, found `Vec<u8>`",
                          f"crates/{d}/src/{f}.rs", 1))
        for d, f in (("a", "x"), ("b", "y"), ("c", "z"))
    ]
    errors, _, _ = parse_cargo_messages(lines, owners, REPO)
    clusters = cluster_errors(errors)
    assert len(clusters) == 1
    assert clusters[0].leases == ["A-01", "B-01", "C-01"]
    assert clusters[0].root_hint == "interface_drift"


def test_three_leases_without_external_path_stays_local():
    owners = {"crates/a/src/x.rs": "A-01", "crates/b/src/y.rs": "B-01",
              "crates/c/src/z.rs": "C-01"}
    lines = [
        json.dumps(_error("pgarbor-wire", "E0425", "cannot find value `x` in this scope",
                          f"crates/{d}/src/{f}.rs", 1))
        for d, f in (("a", "x"), ("b", "y"), ("c", "z"))
    ]
    errors, _, _ = parse_cargo_messages(lines, owners, REPO)
    assert cluster_errors(errors)[0].root_hint == "local"


def test_std_path_is_not_external():
    d = Diagnostic(level="error", code="E0308",
                   message="expected `std::string::String`, found `i32`", file="", line=0,
                   crate="pgarbor-core", lease_id="")
    assert cluster_errors([d, d, d])[0].root_hint == "local"


def test_no_errors_gives_no_clusters():
    assert cluster_errors([]) == []


def test_build_report_json_round_trip_shape():
    errors, warnings, success = parse_cargo_messages(_canned_lines(), OWNERS, REPO)
    report = BuildReport(ok=success, duration_s=1.5, rustc="rustc 1.90.0", cargo="cargo 1.90.0",
                         errors=errors, warnings_by_crate=warnings,
                         clusters=cluster_errors(errors), log_path=None)
    data = report.to_json()
    assert data["ok"] is False
    assert data["warnings_by_crate"] == {"pgarbor-core": 1}
    assert data["clusters"][0]["code"] == "E0308"
    assert data["errors"][0]["lease_id"] in {"STO-01", "WIRE-02", "FLT-01"}


def test_parse_test_output_verdicts():
    text = "\n".join([
        "running 4 tests",
        "test storage::page::roundtrip ... ok",
        "test wal::replay ... FAILED",
        "test fault::slow ... ignored, needs the oracle",
        "test codec::bounds ... ok",
        "",
        "failures:",
        "---- wal::replay stdout ----",
        "test result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out",
    ])
    passed, failed, ignored, ran = _parse_test_output(text)
    assert passed == ["storage::page::roundtrip", "codec::bounds"]
    assert failed == ["wal::replay"]
    assert ignored == ["fault::slow"]
    assert ran is True


def test_parse_test_output_without_tests_is_not_ran():
    passed, failed, ignored, ran = _parse_test_output("error[E0432]: unresolved import\n")
    assert passed == [] and failed == [] and ignored == []
    assert ran is False


def test_parse_test_output_empty_binary_counts_as_ran():
    _, _, _, ran = _parse_test_output("test result: ok. 0 passed; 0 failed\n")
    assert ran is True
