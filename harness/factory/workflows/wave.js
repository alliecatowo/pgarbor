export const meta = {
  name: 'pgarbor-speculative-wave',
  description: 'RightShift Phase B: one worker per file lease in isolated worktrees, static review, one guided retry',
  phases: [
    { title: 'Implement', detail: 'haiku-first workers, one lease each, no builds' },
    { title: 'Review', detail: 'read-only static contract review of each patch' },
    { title: 'Retry', detail: 'one guided retry for patches flagged as fake/broken' },
  ],
}

// args: { epoch: 'epoch-01', base_sha: '<sha>', leases: [ {lease_id, crate, files, model_tier, summary, spec, reads, task_ids, acceptance} ] }
const ROOT = '/home/allie/develop/pgarbor'
const REG = '~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f'
const EPOCH = args.epoch
const MODEL = { haiku: 'haiku', sonnet: 'sonnet', opus: 'sonnet' }

const SUBMISSION = {
  type: 'object',
  properties: {
    lease_id: { type: 'string' },
    branch: { type: 'string', description: 'output of git rev-parse --abbrev-ref HEAD' },
    sha: { type: 'string', description: 'output of git rev-parse HEAD after your commit' },
    files_changed: { type: 'array', items: { type: 'string' } },
    implemented_behaviors: { type: 'array', items: { type: 'string' } },
    still_unimplemented: { type: 'array', items: { type: 'string' }, description: 'items left returning PgError::unimplemented, with reason' },
    assumptions: { type: 'array', items: { type: 'string' } },
    contract_breaks: { type: 'array', items: { type: 'string' }, description: 'places where the frozen contract seemed wrong/insufficient (you must NOT edit other files)' },
    proposed_tests_unexecuted: { type: 'array', items: { type: 'string' } },
    provenance: { type: 'array', items: { type: 'string' }, description: 'PG source files / docs consulted' },
  },
  required: ['lease_id', 'branch', 'sha', 'files_changed', 'implemented_behaviors', 'still_unimplemented', 'assumptions', 'contract_breaks', 'proposed_tests_unexecuted', 'provenance'],
}

const REVIEW = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['ok', 'minor', 'redo'] },
    off_lease_files: { type: 'array', items: { type: 'string' } },
    fake_or_stub: { type: 'array', items: { type: 'string' } },
    contract_drift: { type: 'array', items: { type: 'string' } },
    likely_compile_errors: { type: 'array', items: { type: 'string' } },
    semantic_bugs: { type: 'array', items: { type: 'string' } },
  },
  required: ['verdict', 'off_lease_files', 'fake_or_stub', 'contract_drift', 'likely_compile_errors', 'semantic_bugs'],
}


const COMPANION = '/home/allie/.claude/plugins/cache/openai-codex/codex/1.0.6/scripts/codex-companion.mjs'
// Thin haiku pass-through to Codex (project agents can't be registered mid-session, so the wrapper is inline).
function viaCodex(model, write, task, commitFiles, epochLease) {
  return `You are a thin forwarding wrapper around the Codex CLI companion. Do NOT inspect the repo, reason about code, review, or edit files yourself. Use only Bash.
1. Write the task text below (between the TASK markers, verbatim) to a file: cat > .codex-task.txt <<'CODEX_EOF' ... CODEX_EOF
2. Start: node ${COMPANION} task --background ${write ? '--write ' : ''}--model ${model} "$(cat .codex-task.txt)"   and capture the job id (task-...). Then rm .codex-task.txt
3. Wait (repeat this Bash call until done, max 8 times): for i in $(seq 1 55); do s=$(node ${COMPANION} status <job-id> --json); echo "$s" | grep -qE '"status": *"(completed|failed|cancelled)"' && break; sleep 10; done; echo "$s" | head -c 800
4. node ${COMPANION} result <job-id>
${commitFiles ? `5. git add -- ${commitFiles.join(' ')} && git commit -qm "${epochLease}: codex ${model}"; git rev-parse --abbrev-ref HEAD; git rev-parse HEAD   (if nothing changed, skip the commit)` : ''}
Then return the requested structured output, filled ONLY from Codex's result text${commitFiles ? ' and the git output (never invent a sha)' : ''}.
TASK:
${task}
END TASK`
}

function workerPrompt(l, extra) {
  return `You are an implementation worker in the PGArbor RightShift swarm (${EPOCH}). You are inside an ISOLATED git worktree of the repo (your cwd). PGArbor is a Rust single-file branch-native database targeting exact PostgreSQL 18 behavior.

YOUR LEASE: ${l.lease_id} (crate ${l.crate})
FILES YOU OWN (edit ONLY these): ${l.files.join(', ')}
FULL LEASE SPEC: read your entry (lease_id ${l.lease_id}) in contracts/leases/${l.crate}.json — its summary, spec, reads, depends_on_leases and acceptance are binding.
READ FOR CONTEXT: contracts/ARCHITECTURE.md, contracts/crates/${l.crate}.md, the lease's 'reads' files, and the doc comments in your files (they are the frozen spec). Read the upstream crate source you call into so your calls match real signatures exactly. Dependency sources (exact versions): ${REG}/ . PostgreSQL 18.6 source for reference behavior: ~/develop/reference/postgres-18 (read to understand; write original Rust, never paste C). Reference catalog data: contracts/reference/pg18/*.csv.

RULES (RightShift):
- Do NOT run cargo, rustc, tests, docker/podman, or postgres. Nobody compiles per worker; a central build happens later. So write code carefully: exact types, imports, lifetimes, error conversions, match exhaustiveness, borrowck-friendly. Re-read your code once for compile errors before committing.
- Keep every public signature marked "// CONTRACT" exactly as declared. You may add private helpers, private types and a #[cfg(test)] mod tests in your files. If the contract is insufficient, note it in contract_breaks and work around it inside your files; never edit other files.
- Replace every PgError::unimplemented("${l.lease_id}", ...) body you can with a real, PostgreSQL-faithful implementation. Only leave unimplemented(...) for parts genuinely out of reach, and list them. NEVER todo!(), unimplemented!(), panic! on user/file data, fake data, or silent no-ops. Exact PG error SQLSTATEs and message text.
- Write unit tests (#[cfg(test)]) and, if your lease lists a tests/ file, integration tests there — they will be executed centrally, not by you.
- Prefer clear, fast, allocation-conscious code.
${extra || ''}
FINISH: git add ONLY your leased files; git commit -qm "${EPOCH} ${l.lease_id}: <one-line summary>"; then report branch + sha (git rev-parse --abbrev-ref HEAD; git rev-parse HEAD) and the structured submission.`
}

function reviewPrompt(l, sub) {
  return `Read-only static reviewer for the PGArbor RightShift swarm. Do NOT run cargo or modify anything. Review commit ${sub.sha} for lease ${l.lease_id} (owned files: ${l.files.join(', ')}) in repo ${ROOT}: run \`git -C ${ROOT} show --stat ${sub.sha}\` and \`git -C ${ROOT} show ${sub.sha}\`. Compare against the frozen contract (doc comments + "// CONTRACT" signatures at the parent commit: git -C ${ROOT} show ${sub.sha}^:<file>, contracts/crates/${l.crate}.md) and upstream APIs it calls (read their source in ${ROOT}/crates). Report: files outside the lease; fake/stub behavior (hardcoded outputs, empty results, swallowed errors, todo!/panic!, silent no-ops); contract drift (changed signatures); likely compile errors (wrong method names/types/imports vs actual upstream source); semantic bugs vs PostgreSQL 18. verdict: ok | minor (compile/semantic nits the central repair can fix) | redo (fake/stub-heavy, off-contract, or largely missing).`
}

const results = await pipeline(
  args.leases,
  (l) => agent(args.impl_codex ? viaCodex(args.impl_codex, true, workerPrompt(l).replace(/FINISH:[\s\S]*$/, 'FINISH: do not run git; the wrapper commits. End with a summary of what is implemented, still unimplemented, assumptions, contract problems.'), l.files, `${EPOCH} ${l.lease_id}`) : workerPrompt(l), { label: `impl:${l.lease_id}`, phase: 'Implement', model: 'haiku', isolation: 'worktree', schema: SUBMISSION }),
  async (sub, l) => {
    // review only when a codex reviewer is configured (args.review_agent, e.g. 'codex-sol-review'); haiku never reviews
    if (!args.review_codex || !sub || !sub.sha) return { lease: l, sub, review: null }
    const review = await agent(viaCodex(args.review_codex, false, reviewPrompt(l, sub)), { label: `review:${l.lease_id}`, phase: 'Review', model: 'haiku', schema: REVIEW })
    return { lease: l, sub, review }
  },
  async (r, l) => {
    const needRetry = !r.sub || !r.sub.sha || (r.review && r.review.verdict === 'redo')
    if (!needRetry) return r
    const notes = r.review ? JSON.stringify(r.review).slice(0, 6000) : 'previous attempt produced no commit'
    const prior = r.sub && r.sub.sha ? `A previous attempt exists at commit ${r.sub.sha}: start from it with \`git checkout ${r.sub.sha} -- ${l.files.join(' ')}\` and fix it.` : ''
    const tier = 'haiku'  // max-savings: sonnet only identifies (review), haiku always fixes
    const sub2 = await agent(args.impl_codex ? viaCodex(args.impl_codex, true, workerPrompt(l, `RETRY: ${prior}\nReviewer findings to address: ${notes}`).replace(/FINISH:[\s\S]*$/, 'FINISH: do not run git; the wrapper commits.'), l.files, `${EPOCH} ${l.lease_id} retry`) : workerPrompt(l, `RETRY: ${prior}\nReviewer findings to address: ${notes}`), { label: `retry:${l.lease_id}`, phase: 'Retry', model: MODEL[tier || 'sonnet'], isolation: 'worktree', schema: SUBMISSION })
    return { lease: l, sub: sub2 || r.sub, review: r.review, retried: true }
  },
)

const out = results.filter(Boolean).map((r) => ({
  lease_id: r.lease.lease_id, crate: r.lease.crate, files: r.lease.files,
  sha: r.sub && r.sub.sha, branch: r.sub && r.sub.branch,
  verdict: r.review && r.review.verdict, retried: !!r.retried,
  still_unimplemented: r.sub && r.sub.still_unimplemented,
  contract_breaks: r.sub && r.sub.contract_breaks,
  assumptions: r.sub && r.sub.assumptions,
  proposed_tests_unexecuted: r.sub && r.sub.proposed_tests_unexecuted,
  provenance: r.sub && r.sub.provenance,
  review: r.review,
}))
const missing = args.leases.filter((l) => !out.find((o) => o.lease_id === l.lease_id && o.sha)).map((l) => l.lease_id)
log(`submitted ${out.filter((o) => o.sha).length}/${args.leases.length}; missing: ${missing.join(', ')}`)
return { epoch: EPOCH, submissions: out, missing }
