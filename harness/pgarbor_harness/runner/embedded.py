"""Socket-free embedded differential campaign; preserves the recorded comparator policy.

One fresh Rust process/database per case. SQL and reconnect steps only; protocol,
async scheduling, restart and capability-dependent cases are explicitly BLOCKED.
This is SQL API evidence, never wire/client compatibility evidence.
"""
from __future__ import annotations
import argparse
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
from pathlib import Path
import subprocess
import time
from ..cases.loader import load_corpus
from ..compare.classify import classify
from ..compare.normalize import has_legacy_oid_values
from ..compare.typed import compare_transcripts
from ..config import DIFFERENTIAL_SESSION_GUCS
from ..model import ExpectedFile, Transcript, expected_path


def run_case(case, binary: Path, root: Path, out: Path, timeout: float):
    record = {"case_id": case.id, "family": case.family, "selected": 1,
              "reference_valid": False, "executed": False, "transport": "embedded"}
    def verdict(outcome, reason):
        return dict(record, outcome=outcome, reason=reason)
    if case.quarantine is not None:
        return verdict("INVALID_CASE", "quarantined")
    path = expected_path(root / "tests/expected", case.id)
    if not path.is_file():
        return verdict("BLOCKED", "reference_missing")
    expected = ExpectedFile.load(path)
    if expected.case_sha256 != case.sha256:
        return verdict("BLOCKED", "reference_stale")
    if has_legacy_oid_values(expected.transcript):
        return verdict("BLOCKED", "reference_normalization_stale")
    record["reference_valid"] = True
    if any(s.is_async or s.kind not in ("sql", "reconnect") for s in case.steps):
        return verdict("BLOCKED", "embedded_step_capability")
    missing = set(case.required_capabilities) & {"server-restart", "extended-protocol", "wire-protocol"}
    if missing:
        return verdict("BLOCKED", "embedded_required_capability:" + sorted(missing)[0])
    request = {"id": case.id, "setup": case.setup, "gucs": DIFFERENTIAL_SESSION_GUCS,
               "steps": [dict(s.payload, kind=s.kind, session=s.session) for s in case.steps]}
    start = time.monotonic()
    record["executed"] = True
    try:
        proc = subprocess.run([str(binary)], input=json.dumps(request), text=True,
                              capture_output=True, timeout=timeout, cwd=root)
    except subprocess.TimeoutExpired:
        return verdict("HANG", "embedded_process_timeout")
    record["duration_ms"] = int((time.monotonic() - start) * 1000)
    if proc.returncode:
        (out / "errors" / (case.id + ".stderr")).write_text(proc.stderr)
        return verdict("CRASH", f"embedded_exit:{proc.returncode}")
    response = json.loads(proc.stdout)
    if "bridge_error" in response:
        record["executed"] = False  # no complete transcript, not SQL compatibility evidence
        record["bridge_error"] = response["bridge_error"]
        return verdict("HARNESS_ERROR", "embedded_transport_error")
    transcript = Transcript.from_json(response["transcript"])
    result = classify(case, expected.transcript, transcript,
                      compare_transcripts(case, expected.transcript, transcript))
    (out / "transcripts" / (case.id + ".json")).write_text(json.dumps(transcript.to_json(), indent=2) + "\n")
    return dict(record, outcome=result.outcome.value, reason=result.reason,
                signature=result.signature,
                diffs=[d.to_json() for d in result.diffs[:10]])


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--binary", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--family", action="append", default=[])
    p.add_argument("--jobs", type=int, default=4)
    p.add_argument("--timeout", type=float, default=5)
    args = p.parse_args(argv)
    root = Path(__file__).resolve().parents[3]
    binary = args.binary.resolve()
    out = args.out.resolve()
    if out.exists() and any(out.iterdir()):
        p.error("output directory must be fresh")
    for name in ("transcripts", "errors"):
        (out / name).mkdir(parents=True, exist_ok=True)
    corpus = load_corpus(root / "tests/cases", root)
    cases = [c for c in corpus.cases if not args.family or c.family in args.family]
    if not cases:
        p.error("selection is empty")
    import resource
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    families = defaultdict(Counter)
    start = time.monotonic()
    with (out / "results.jsonl").open("w") as log, ThreadPoolExecutor(max_workers=args.jobs) as pool:
        def execute(c):
            try:
                return run_case(c, binary, root, out, args.timeout)
            except Exception as e:
                return dict(case_id=c.id, family=c.family, selected=1, executed=False,
                            reference_valid=False, outcome="HARNESS_ERROR", reason=repr(e))
        for i, record in enumerate(pool.map(execute, cases), 1):
            log.write(json.dumps(record) + "\n"); log.flush()
            f = families[record["family"]]
            f.update({"selected": 1, "reference_valid": int(record["reference_valid"]),
                      "executed": int(record["executed"]), record["outcome"]: 1})
            if i % 250 == 0:
                print(f"{i}/{len(cases)} cases accounted", flush=True)
    summary = dict(transport="embedded API, no wire", binary=str(binary),
                   binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                   elapsed_seconds=round(time.monotonic() - start, 2), invalid_files=len(corpus.invalid),
                   selected=len(cases), families=dict(families),
                   reference="existing recorded PG18 files; no live oracle verification or rerecord")
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return 0 if all(f["PASS"] == f["selected"] for f in families.values()) else 1

if __name__ == "__main__":
    raise SystemExit(main())
