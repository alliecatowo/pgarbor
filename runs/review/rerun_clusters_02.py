"""Recheck the 01 CRASH/UNEXPECTED_ACCEPTANCE cases using the unchanged embedded runner."""
import json
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from pgarbor_harness.cases.loader import load_corpus
from pgarbor_harness.runner.embedded import run_case
root=Path(__file__).resolve().parents[2]
old=[json.loads(l) for l in (root/'runs/review/embedded-final-01/results-verified.jsonl').read_text().splitlines()]
ids={r['case_id'] for r in old if r['outcome'] in ('CRASH','UNEXPECTED_ACCEPTANCE')}
out=root/'runs/review/embedded-clusters-02'
for name in ('transcripts','errors'): (out/name).mkdir(parents=True,exist_ok=True)
cases=[c for c in load_corpus(root/'tests/cases',root).cases if c.id in ids]
with ThreadPoolExecutor(max_workers=4) as pool:
 results=list(pool.map(lambda c: run_case(c,root/'target/debug/examples/embedded_case',root,out,20),cases))
(out/'results.jsonl').write_text(''.join(json.dumps(r)+'\n' for r in results))
print(Counter(r['outcome'] for r in results))
