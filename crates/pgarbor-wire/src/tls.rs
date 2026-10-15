//! TLS and GSS encryption: explicitly NOT implemented yet (WIRE-13). Fully implemented, frozen (v1).
//!
//! * An `SSLRequest` is answered with the single byte `N` ("server does not support SSL"). libpq with
//!   `sslmode=prefer`/`allow` then continues in plaintext; `require`/`verify-*` fail client-side with
//!   `server does not support SSL, but SSL was required`.
//! * A `GSSENCRequest` is answered with `N` as well.
//! * Because there is no TLS, the server binds loopback / Unix sockets by default, refuses
//!   non-loopback binds without an explicit opt-in (`ServerConfig::allow_nonloopback`) and refuses
//!   trust authentication there (see `config`). Passwords on a non-loopback plaintext link are
//!   protected only by SCRAM; this is documented as a known gap.
//! * Drivers: add `sslmode=disable` to URLs the CLI prints so no driver attempts negotiation.
use crate::config::TlsPolicy;

/// The one-byte answer to SSLRequest / GSSENCRequest under `policy`.
pub fn encryption_answer(policy: TlsPolicy) -> u8 {
    match policy {
        TlsPolicy::Disabled | TlsPolicy::Required => b'N',
    }
}
