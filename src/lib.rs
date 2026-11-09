//! crane-rs: container registry client and registry server.
//! Types: crane.ts (canonical) -> src/types.rs.

pub mod auth;
pub mod cli;
pub mod client;
pub mod commands;
pub mod digest;
#[doc(hidden)]
pub mod fixture;
pub mod local;
pub mod reference;
pub mod registry;
pub mod transfer;
pub mod types;

#[cfg(test)]
pub(crate) mod testutil;
