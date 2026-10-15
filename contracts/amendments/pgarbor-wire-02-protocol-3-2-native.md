Status: ACCEPTED (lead-engineer decision, 2026-10-15).
Crate: pgarbor-wire
Affects: contracts/ARCHITECTURE.md section 12, pgarbor-wire, pgarbor (SessionOptions.cancel_key_len)
Leases: WIR-03 (startup packet), WIR-08 (startup flow)

# pgarbor-wire-02: speak protocol 3.2 natively (deviation from ARCHITECTURE section 12)

ARCHITECTURE section 12 says "accept 3.2 negotiation via NegotiateProtocolVersion down to 3.0".
The pinned reference (PostgreSQL 18.6) itself accepts 3.2 natively: BackendKeyData carries a
variable-length secret (4..=256 bytes; PG sends 32) and CancelRequest has a variable-length secret.
Wire transcript parity (WIRE-12) is measured against PG18, so PGArbor does the same:

* client requests 3.0 -> 4-byte cancel secret (`SessionOptions.cancel_key_len = 4`);
* client requests 3.2 -> 32-byte cancel secret, CancelRequest packet length 16..=264;
* client requests 3.x with x > 2 (or `_pq_.*` options) -> NegotiateProtocolVersion(newest_minor = 2,
  unrecognized option names) then normal operation at 3.2;
* major != 3 -> FATAL 0A000 `unsupported frontend protocol N.M: server supports 3.0 to 3.2`.

Section 12 is amended accordingly; no frozen Rust item changes.

## Steward decision (2026-10-15)

Accept native 3.0 and 3.2 protocol representations implemented by startup_flow/startup_packet. Protocol conformance remains unverified without live PG18 and independent client execution; no oracle results changed.
