//! Source-only immutable environment. No backend/model/profile selection is
//! made here; actual registered/config/image/CAS owners remain held.
pub(crate) use crate::managed_machine_runtime_profile::fresh_environment::ManagedFreshEnvironment;
