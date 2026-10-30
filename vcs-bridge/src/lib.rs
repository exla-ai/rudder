//! Private jj metadata and typed filesystem/history publication boundaries.
//!
//! This crate is not a scheduler or an authority journal. The engine owns
//! retention and the final owner/run/decision integration gate. jj support and
//! its CLI are explicit features; ordinary Rudder does not switch jj binaries.

mod atomic;
#[cfg(feature = "jj")]
pub mod attempt;
pub mod identity;
#[cfg(feature = "jj")]
pub mod journal_adapter;
#[cfg(feature = "local-development")]
pub mod local_development;
#[cfg(feature = "jj")]
pub mod publication;
#[cfg(feature = "jj")]
pub mod projection_io;
#[cfg(feature = "jj")]
pub mod projection;
#[cfg(feature = "jj")]
pub mod vfs_provider;
#[cfg(feature = "jj")]
pub mod rpc_view;
#[cfg(feature = "jj")]
pub mod command_transport;
#[cfg(feature = "jj")]
pub mod host;
pub mod seed;
#[cfg(feature = "jj")]
pub mod working_copy;

pub use identity::*;
