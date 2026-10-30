//! Rudder's versioned workspace and durable machine-engine contracts.
//!
//! Local journals are authoritative only for explicitly local projects. They
//! are not a replicated project authority and must not be placed on shared NFS.

pub mod protocol;
pub mod paths;
pub mod store;
pub mod journal;
pub mod service;
pub mod agent_api;
pub mod workspace;
pub mod build;
pub mod execution;
pub mod vfs;
