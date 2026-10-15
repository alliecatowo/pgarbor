# Refuse unenforced row security and deferred constraints

Status: ACCEPTED (lead-engineer decision, 2026-10-15).

The existing executor contract's promise to check deferred constraints early is rejected:
that changes SQL semantics. CREATE/ALTER requesting deferral returns EXE-33 leased 0A000.
Enabling/forcing row security likewise returns EXE-33 0A000 until policies are enforced.
A previously stored relation with security flags or deferred constraints is rejected by
EXE-01 table description, including reads, rather than exposing unfiltered rows or
pretending early checking implements deferral. Removing/disabling flags stays allowed.
This is a compatibility limitation, covered by SQL session acceptance tests.
