//! Fixed retention substitution for an actual acknowledged Runnable DAG. No
//! supplied paths, generic callbacks or serialized receipts authorize roots.
use super::*;
use crate::store::{ImageImportFence,environment_dependencies::{RetainedRunnableInputs,RetainedRunnableRead}};
use std::sync::Arc;
pub struct VcsRetainedInputs{owner:RetainedRunnableInputs,original:Arc<dyn ImageImportFence>,read:Option<RetainedRunnableRead>}
impl VcsRetainedInputs{
    pub fn stage_retaining(out:&mut Option<Self>,owner:RetainedRunnableInputs,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original VCS retained inputs occupied");*out=Some(Self{owner,original,read:None});Ok(())}
    pub fn inputs(&self)->&RetainedRunnableInputs{&self.owner}
    pub(super) fn check(&mut self,store:&LocalStore)->Result<()> {
        self.original.remaining()?;ensure!(store.root()==self.owner.store_root_data()&&self.read.is_none(),"actual VCS store changed or failed input read remains");
        self.owner.read_retaining(&mut self.read,self.owner.root_data().clone(),self.original.clone())?;
        self.read.as_ref().unwrap().verified_bytes()?;self.original.remaining()?;self.read.take();Ok(())}
    pub(super) fn retain(&mut self,store:&LocalStore,id:&str,roots:Vec<ObjectRef>)->Result<BTreeMap<String,ObjectRef>>{
        self.check(store)?;let mut pins=BTreeMap::new();for root in roots{self.original.remaining()?;
            if self.owner.contains_data(&root,&*self.original)?{
                // Record the ACTUAL durable containing pin, never invent a
                // LocalPin for a foreign leaf or an alias with no retention.
                self.owner.read_retaining(&mut self.read,root,self.original.clone())?;self.read.as_ref().unwrap().verified_bytes()?;
                self.original.remaining()?;self.read.take();pins.insert(self.owner.pin_name_data().into(),self.owner.root_data().clone());
            }else{pins.extend(super::retain(store,id,vec![root])?);}
        }self.check(store)?;Ok(pins)}
}
