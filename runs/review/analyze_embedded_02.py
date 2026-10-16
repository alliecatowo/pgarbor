"""Summarize full campaigns without changing raw results, cases, references, or policy."""
import csv, hashlib, json
from collections import Counter, defaultdict
from pathlib import Path
root=Path(__file__).resolve().parents[2]
review=root/'runs/review'
old=[json.loads(l) for l in (review/'embedded-final-01/results-verified.jsonl').read_text().splitlines()]
new=[json.loads(l) for l in (review/'embedded-final-02/results.jsonl').read_text().splitlines()]
assert len(new)==5837 and len({r['case_id'] for r in new})==5837
assert {r['case_id'] for r in old}=={r['case_id'] for r in new}
by_old={r['case_id']:r for r in old}
by_family=defaultdict(lambda: [Counter(),Counter()])
for i,rows in enumerate([old,new]):
 for r in rows:
  by_family[r['family']][i].update({'selected':1,r['outcome']:1})
fields=['family','selected_01','pass_01','pass_rate_01_pct','selected_02','pass_02','pass_rate_02_pct','pass_delta','rate_delta_pp']
with (review/'codex-differential-by-family-02.csv').open('w') as f:
 w=csv.writer(f);w.writerow(fields)
 for family,(a,b) in sorted(by_family.items()):
  pa=100*a['PASS']/a['selected'];pb=100*b['PASS']/b['selected']
  w.writerow([family,a['selected'],a['PASS'],f'{pa:.4f}',b['selected'],b['PASS'],f'{pb:.4f}',b['PASS']-a['PASS'],f'{pb-pa:.4f}'])
transitions=Counter((by_old[r['case_id']]['outcome'],r['outcome']) for r in new)
regressions=[r['case_id'] for r in new if by_old[r['case_id']]['outcome']=='PASS' and r['outcome']!='PASS']
clusters={}
for outcome in ['MISMATCH','CRASH','UNEXPECTED_ACCEPTANCE','UNSUPPORTED']:
 c=Counter()
 for r in new:
  if r['outcome']!=outcome:continue
  sig=r.get('signature') or {};key=sig.get('message_template') or sig.get('diff_kind') or r['reason']
  c[key]+=1
 clusters[outcome]=c.most_common()
old_priority={r['case_id'] for r in old if r['outcome'] in ('CRASH','UNEXPECTED_ACCEPTANCE')}
metrics={
 'baseline_counts':dict(Counter(r['outcome'] for r in old)),
 'final_counts':dict(Counter(r['outcome'] for r in new)),
 'selected':len(new),'reference_valid':sum(r['reference_valid'] for r in new),
 'executed':sum(r['executed'] for r in new),
 'pass_rate_selected_pct':100*sum(r['outcome']=='PASS' for r in new)/len(new),
 'pass_rate_executed_pct':100*sum(r['outcome']=='PASS' for r in new)/sum(r['executed'] for r in new),
 'priority_160_counts':dict(Counter(r['outcome'] for r in new if r['case_id'] in old_priority)),
 'transitions':[{'from':a,'to':b,'count':n} for (a,b),n in sorted(transitions.items())],
 'previous_pass_regressions':regressions,'clusters':clusters,
 'binary_sha256':json.load((review/'embedded-final-02/summary.json').open())['binary_sha256'],
}
# The outcome classifier alone does not cover bridge errors. Count both explicitly.
internal=Counter(); bridge_internal=Counter()
for record in new:
 path=review/'embedded-final-02/transcripts'/(record['case_id']+'.json')
 if path.exists():
  transcript=json.load(path.open())
  for step in transcript['setup']+transcript['steps']:
   for result in step.get('results',[]):
    error=result.get('error') or {}
    if error.get('sqlstate','').startswith('XX'):internal[error['sqlstate']]+=1
 error=record.get('bridge_error')
 if isinstance(error,dict) and error.get('sqlstate','').startswith('XX'):bridge_internal[error['sqlstate']]+=1
metrics['xx_errors_in_complete_transcripts']=dict(internal)
metrics['xx_errors_in_bridge_failures']=dict(bridge_internal)
metrics['process_failure_cases']=[r['case_id'] for r in new if r.get('reason','').startswith('embedded_exit:')]
metrics['timeout_cases']=[r['case_id'] for r in new if r['outcome']=='HANG']
(review/'codex-differential-summary-02.json').write_text(json.dumps(metrics,indent=2)+'\n')
print(json.dumps({k:v for k,v in metrics.items() if k not in ('transitions','clusters')},indent=2))
