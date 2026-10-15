"""PGArbor verification harness: pinned PG18 oracle, case corpus, differential runner,
exact comparator, official-suite runners, minimizer and verifier self-tests.

Contract: contracts/crates/harness.md. Frozen shared modules (never leased): config, model,
hashing, cases.model. Every other module is one lease (contracts/leases/harness.json).
Importing this package needs only the stdlib (+ PyYAML for cases.loader); psycopg is imported
lazily inside runner/oracle functions.
"""

__version__ = "0.1.0"
