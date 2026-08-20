//! Connection identity. `ConnId` is a `gateway` type, not a `core` one —
//! passed through the channels opaquely, never seen by `Engine` (SPEC §4).

/// Identifies one accepted order-entry connection. Assigned by the accept
/// loop, carried alongside `Command`/`Event` on the channels between the
/// gateway and matching threads, and used only to route a reply back to
/// the connection it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnId(pub u64);
