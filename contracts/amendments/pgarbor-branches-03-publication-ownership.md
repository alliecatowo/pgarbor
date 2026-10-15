# Branches 03: guarded export and detach publication

Status: ACCEPTED (2026-10-15, lead engineer delegation to Codex storage safety owner).

The fixed intermediate `.detach-tmp` pathname must be protected for the complete lifecycle,
including the gap between ExportBuilder finish, verification reopen and final publication.
Both copy export and reflink detach acquire an exclusive `.detach-guard` sibling using
CreateNew. A preexisting guard can be reclaimed only after obtaining its exclusive lock and
revalidating identity; a creator paused before locking likewise revalidates before proceeding.
Changing guards are retried at most 16 times. Contention returns 55006.

While holding this stable destination guard, stale intermediate cleanup opens, exclusively
locks and identity-checks the intermediate before unlinking. A live intermediate is preserved.
Construction failures clean only under the guard. Guard cleanup retains its own lock until
unlink completes. A second builder cannot remove or reuse the first builder's temporary name.

Verification keeps the Container open and locked until rename and directory sync finish.
With overwrite=false, the final move uses Vfs::rename_noreplace; AlreadyExists becomes 58P02
and preserves the other destination even if it appeared after the last existence check.
The public verify_and_publish helper acquires the same guard; internal guarded callers use a
crate-private variant. Source branch data and intermediate format are unchanged.

This supersedes BR-10/BR-11 lease instructions allowing unconditional stale-temp unlink and
ordinary final rename. Stale crash leftovers are still reclaimed when unlocked and owned
under the guard. Reflink's CreateNew target rule remains unchanged.
