//! Rudder's versioned workspace and durable machine-engine contracts.
//!
//! Local journals are authoritative only for explicitly local projects. They
//! are not a replicated project authority and must not be placed on shared NFS.

pub mod protocol;
pub mod paths;
pub mod store;
pub mod content;
pub mod remote_cache;
pub mod remote_source;
pub mod remote_root;
pub mod delta;
pub mod directory;
pub mod snapshot;
pub mod tree;
pub mod journal;
pub mod service;
pub mod agent_api;
pub mod workspace;
pub mod build;
pub mod execution;
pub mod vfs;
