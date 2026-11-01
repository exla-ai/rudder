use super::*;
use crate::build::{backed_preparation::RetainedBackedPreparation,environment_preparation_lineage::EnvironmentPreparationLineage};
fn lineage_id(file:&File)->Result<(u64,u64,u32,u32,u32)>{let m=file.metadata()?;ensure!(m.is_dir()&&m.uid()==unsafe{libc::geteuid()}&&m.mode()&0o7777==0o700,"original environment index directory not private owner");Ok((m.dev(),m.ino(),m.mode(),m.uid(),m.gid()))}
impl RetainedBackedPreparation{
    /// Original ACK prerequisite, exact prepared Environment only. No input
    /// path, ObjectRef, saved receipt or caller bool chooses another boundary.
    pub fn derive_environment_subgraph_retaining(&self,out:&mut Option<Arc<crate::store::environment_dependencies::RetainedEnvironmentSubgraph>>,original:Arc<dyn ImageImportFence>)->Result<()> {
        self.acknowledgment()?;
        crate::store::environment_dependencies::RetainedEnvironmentSubgraph::capture_from_preparation_retaining(out,self,original)
    }
    pub(crate) fn capture_environment_lineage_retaining(&self,out:&mut Option<Arc<EnvironmentPreparationLineage>>,original:&dyn ImageImportFence)->Result<()> {
        ensure!(out.is_none(),"original environment lineage already staged");let ack=self.acknowledgment()?;
        *out=Some(Arc::new(EnvironmentPreparationLineage{index:ack.index.clone(),parent:ack.retention.clone(),environment:ack.prepared.root.clone(),receipt:ack.prepared.receipt.clone(),directories:[None,None],ids:[None,None],ready:false}));
        let lineage=Arc::get_mut(out.as_mut().unwrap()).context("original environment lineage aliased during capture")?;
        // Do not call core.check()/old clock: this is a NEW legitimate bounded
        // derivation of the already acknowledged immutable preparation.
        original.remaining()?;self.core.witness.check(&self.core.index)?;
        for(index,source)in [self.core.witness.root.as_ref(),self.core.witness.locks.as_ref()].into_iter().enumerate(){
            original.remaining()?;lineage.directories[index]=Some(source.context("original preparation directory witness absent")?.try_clone()?);
            lineage.ids[index]=Some(lineage_id(lineage.directories[index].as_ref().unwrap())?);
        }
        lineage.check_inner(original)?;lineage.ready=true;original.remaining()?;Ok(())
    }
}
impl EnvironmentPreparationLineage{
    fn check_inner(&self,original:&dyn ImageImportFence)->Result<()> {
        original.remaining()?;
        for(index,path)in [&self.index.root,&self.index.locks].into_iter().enumerate(){
            let file=self.directories[index].as_ref().context("original environment index directory capture incomplete")?;
            let expected=self.ids[index].context("original environment index identity absent")?;
            let named=fs::symlink_metadata(path)?;ensure!(!named.file_type().is_symlink()&&named.is_dir()&&lineage_id(file)?==expected
                &&(named.dev(),named.ino(),named.mode(),named.uid(),named.gid())==expected,"original environment index directory replaced");
        }
        loop{original.remaining()?;match self.index.connection.try_lock(){Ok(keeper)=>{sqlite_binding_current(&keeper,&self.index.root.join("index.sqlite"))?;break;},
            Err(TryLockError::WouldBlock)=>std::thread::sleep(original.remaining()?.min(Duration::from_millis(1))),Err(_)=>anyhow::bail!("original environment SQLite keeper poisoned")}}
        ensure!(self.environment.kind==ObjectKind::EnvironmentRoot&&self.parent.root_data()==&self.receipt,"original preparation receipt/environment lineage differs");original.remaining()?;Ok(())
    }
    pub(crate) fn current(&self,original:&dyn ImageImportFence)->Result<()> {ensure!(self.ready,"original environment lineage ACK absent");self.check_inner(original)}
}
