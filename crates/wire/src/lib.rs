//! Binary wire protocol: message schemas, encode/decode, framing.
//!
//! Depends on `core` and decodes directly into its types — no parallel type
//! hierarchy, no conversion layer (SPEC §4). Built in stage 2.
