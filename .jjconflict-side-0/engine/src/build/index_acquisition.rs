//! Create-only acquisition of the actual shared index keeper. DATA/storage
//! ownership only; no Root/project/source/task permission or restart adoption.
use super::*;
use crate::execution::finalization_budget::FinalizationBudget;
use crate::store::ImageImportFence;
use std::{fs::File,path::PathBuf,sync::{Arc,atomic::AtomicBool}};

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub enum IndexAcquisitionPhase {Captured,Creating,Opening,Configuring,Acknowledged,Failed}
#[must_use="retain original index acquisition on every outcome"]
pub struct RetainedIndexAcquisition {
    pub(in crate::build) requested:PathBuf,pub(in crate::build) store:LocalStore,
    pub(in crate::build) original:FinalizationBudget,pub(in crate::build) cancelled:Arc<AtomicBool>,
    pub(in crate::build) restriction:Arc<dyn ImageImportFence>,pub(in crate::build) phase:IndexAcquisitionPhase,
    pub(in crate::build) parent:Option<File>,pub(in crate::build) root:Option<File>,pub(in crate::build) locks:Option<File>,
    pub(in crate::build) parent_id:Option<(u64,u64)>,pub(in crate::build) root_id:Option<(u64,u64)>,pub(in crate::build) locks_id:Option<(u64,u64)>,
    pub(in crate::build) mkdir_attempted:bool,pub(in crate::build) sqlite_attempted:bool,pub(in crate::build) sql_step:usize,
    pub(in crate::build) connection:Option<rusqlite::Connection>,pub(in crate::build) index:Option<Arc<BuildIndex>>,
    pub(in crate::build) failure:Option<anyhow::Error>,
}
impl RetainedIndexAcquisition {
    pub fn phase_data(&self)->IndexAcquisitionPhase{self.phase}
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    pub fn index(&self)->Result<&Arc<BuildIndex>>{
        ensure!(self.phase==IndexAcquisitionPhase::Acknowledged,"original shared index acquisition ACK absent/unknown");
        self.index.as_ref().context("actual original index keeper absent")
    }
}
