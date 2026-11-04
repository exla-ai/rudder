//! Original preparation namespace/keeper lineage, never a serialized receipt
//! or an execution/project grant. Only actual acknowledged preparation issues
//! this owner. A completed operation's clock is not its immutable fact lifetime.
use super::*;
use crate::store::{ImageImportFence,backed_retention::BackedPinOwner};
use std::{fs::File,sync::Arc};
pub(crate) struct EnvironmentPreparationLineage{
    pub(in crate::build) index:Arc<BuildIndex>,pub(in crate::build) parent:Arc<BackedPinOwner>,
    pub(in crate::build) environment:ObjectRef,pub(in crate::build) receipt:ObjectRef,
    pub(in crate::build) directories:[Option<File>;2],pub(in crate::build) ids:[Option<(u64,u64,u32,u32,u32)>;2],
    pub(in crate::build) ready:bool,
}
impl EnvironmentPreparationLineage{
    pub(crate) fn parent(&self)->&Arc<BackedPinOwner>{&self.parent}
    pub(crate) fn environment(&self)->&ObjectRef{&self.environment}
    pub(crate) fn receipt(&self)->&ObjectRef{&self.receipt}
}
