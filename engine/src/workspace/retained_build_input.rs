//! Actual retained Runnable DATA -> existing BuildInputView qualification.
//! Source-only view; this does not authorize a Root floor, SDK or execution.
use super::*;
use crate::store::{ImageImportFence,environment_dependencies::{RetainedRunnableInputs,RetainedRunnableRead}};
use std::sync::Arc;
pub struct RetainedRunnableBuildInput{store:LocalStore,owner:RetainedRunnableInputs,policy:ObjectRef,budget:ProjectionBudget,original:Arc<dyn ImageImportFence>,
    read:Option<RetainedRunnableRead>,input:Option<crate::build::BuildInputView>,report:Option<ProjectionReport>,attempted:bool,ready:bool,failure:Option<anyhow::Error>}
impl RetainedRunnableBuildInput{
    pub fn capture_retaining(out:&mut Option<Self>,store:LocalStore,owner:RetainedRunnableInputs,policy:ObjectRef,budget:ProjectionBudget,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original retained build input occupied");*out=Some(Self{store,owner,policy,budget,original,read:None,input:None,report:None,attempted:false,ready:false,failure:None});
        let cell=out.as_mut().unwrap();if let Err(e)=cell.capture(){cell.failure=Some(e);anyhow::bail!("retained runnable build qualification failed; keep original read/input owner")};Ok(())
    }
    fn capture(&mut self)->Result<()>{ensure!(!self.attempted,"original retained build input cannot retry");self.attempted=true;self.original.remaining()?;
        let root=self.owner.root_data().clone();ensure!(root.kind==ObjectKind::RunnableRoot&&self.store.root()==self.owner.store_root_data(),"retained build input changed actual Runnable/store");
        self.owner.read_retaining(&mut self.read,root.clone(),self.original.clone())?;self.read.as_ref().unwrap().verified_bytes()?;self.original.remaining()?;
        let began=Instant::now();let(input,report)=qualify_runnable_build_data(&self.store,&root,&self.policy,&self.budget,began,BTreeMap::new(),Some(self.original.clone()),true)?;
        self.input=Some(input);self.report=Some(report);self.original.remaining()?;self.read.take();self.ready=true;Ok(())}
    pub fn input(&self)->Result<&crate::build::BuildInputView>{ensure!(self.ready,"original retained build input ACK absent");self.input.as_ref().context("actual qualified input absent")}
    pub fn report_data(&self)->Option<&ProjectionReport>{self.report.as_ref()}
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    pub fn current_retaining(&mut self,original:Arc<dyn ImageImportFence>)->Result<()> {self.input()?;original.remaining()?;
        ensure!(self.read.is_none(),"original retained build read still unresolved");self.owner.read_retaining(&mut self.read,self.owner.root_data().clone(),original.clone())?;
        self.read.as_ref().unwrap().verified_bytes()?;original.remaining()?;self.read.take();Ok(())}
}
