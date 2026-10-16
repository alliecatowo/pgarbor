"""Write reviewable logical patches; never writes the Git index."""
from pathlib import Path
import hashlib,json,subprocess
root=Path(__file__).resolve().parents[2]
out=root/'runs/review/patches-02';out.mkdir(exist_ok=True)
def files(crate,names): return [f'crates/{crate}/src/{n}.rs' for n in names.split()]
groups=[
 ('fix(sql): enforce PostgreSQL grammar and bind variadic builtin calls',
  files('pgarbor-sql','lib parser strict_check syntax_error binder/expr_func')+files('pgarbor-exec','eval_call')+files('pgarbor-plan','simplify')+['crates/pgarbor/tests/sql_variadic_any_contract.rs','crates/pgarbor/tests/sql_variadic_contract.rs']),
 ('fix(plan): preserve aggregate and join slots across query boundaries',
  files('pgarbor-plan','agg build context cte join planner setrefs sort_limit')+files('pgarbor-sql','binder/expr_agg deparse/query')+['crates/pgarbor/tests/sql_planner_targetlist.rs','contracts/amendments/pgarbor-plan-04-query-level-parentage.md']),
 ('fix(exec): lower ALTER USING expressions and rebind typed defaults',
  files('pgarbor-exec','ddl_followup tabledesc')),
 ('fix(exec): stream recursive CTEs and invalidate changed working tables',
  files('pgarbor-exec','node_cte runtime')+['crates/pgarbor/tests/cte_scope_contract.rs','contracts/amendments/pgarbor-exec-09-recursive-cte-streaming.md']),
 ('fix(exec): evaluate scalar expressions containing set-returning calls',
  files('pgarbor-exec','node_func')),
 ('fix(catalog): retain dependencies and reject invalid utility settings',
  files('pgarbor-catalog','ddl/comment ddl/grant ddl/typedef ddl/alter_table')+files('pgarbor-sql','binder/ddl_function binder/ddl_index binder/ddl_view binder/from binder/utility')+
  files('pgarbor-exec','context ddl_alter exec_session session_fns utility')+files('pgarbor','guc')+
  ['crates/pgarbor/tests/sql_session_contract.rs','contracts/amendments/pgarbor-catalog-08-domain-container-validation.md','contracts/amendments/pgarbor-exec-08-guc-validation.md']),
 ('fix(types): preserve OID representations and reject missing record descriptors',
  files('pgarbor-types','cast/apply agg/ordered_set record')+files('pgarbor-exec','eval eval_cast')),
 ('fix(api): report PostgreSQL session and materialized-view completion tags',
  files('pgarbor','run_stmt')+files('pgarbor-exec','ddl_objects')),
 ('test: record embedded review 02 and semantic regression coverage',
  ['crates/pgarbor/tests/sql_cluster_semantics.rs','STATUS.md','runs/review/codex-final-02.md','runs/review/codex-change-audit-02.json','runs/review/codex-differential-by-family-02.csv','runs/review/codex-differential-summary-02.json','runs/review/codex-test-layers-02.json','runs/review/analyze_embedded_02.py','runs/review/rerun_clusters_02.py','runs/review/prepare_patches_02.py']),
]
commits=[]
seen=set()
for i,(message,paths) in enumerate(groups,1):
 patch=b''
 for name in paths:
  assert name not in seen,name
  seen.add(name)
  if not (root/name).exists(): raise SystemExit('Missing deliverable: '+name)
  tracked=subprocess.run(['git','-c','core.fsmonitor=false','ls-files','--error-unmatch','--',name],cwd=root,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode==0
  args=['git','-c','core.fsmonitor=false','diff','--no-ext-diff','--no-textconv','--src-prefix=a/','--dst-prefix=b/']
  args+=['HEAD','--',name] if tracked else ['--no-index','--','/dev/null',name]
  result=subprocess.run(args,cwd=root,capture_output=True)
  assert result.returncode in (0,1),result.stderr
  patch+=result.stdout
 patch_name=f'{i:02d}.patch'
 (out/patch_name).write_bytes(patch)
 commits.append({'message':message+'\n\nCo-Authored-By: OpenAI Codex <noreply@openai.com>','files':paths,'patch':patch_name,'sha256':hashlib.sha256(patch).hexdigest()})
changed=subprocess.check_output(['git','-c','core.fsmonitor=false','diff','HEAD','--name-only','--','crates','contracts','STATUS.md'],cwd=root,text=True).splitlines()
assert set(changed)<=seen,set(changed)-seen
manifest={'base':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),
 'git_read_only':True,'instructions':'The working tree already contains these changes: stage each listed file group and commit in order, using the complete message including its final Co-Authored-By trailer. Patches are recovery/review artifacts relative to the listed base; do not apply them over the modified working tree. The shared regression target is in the final group. No commits or index writes were attempted.',
 'inherited_snapshot':'inherited.patch (audit backup only; includes rejected changes; do not apply)',
 'commits':commits}
(out/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
print('Wrote',len(commits),'logical patch groups covering',len(seen),'files')
