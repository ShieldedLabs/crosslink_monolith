//! Shielded Transport Pigeon: encrypted, optionally reliable UDP.
//!
//! Extracted unchanged from Tenderlink. Tenderlink and New Network call this
//! crate. The Nym backend in `nym_sockets` is a stub; the previous implementation
//! is commented out in that file.

#![allow(non_snake_case, non_upper_case_globals)]
#![allow(dead_code, unused_variables, unused_mut, unused_imports, unused_assignments, unused_unsafe)]
#![allow(deprecated)]

pub mod helpers;
pub mod native_sockets;
pub mod nym_sockets;
pub mod stp;
