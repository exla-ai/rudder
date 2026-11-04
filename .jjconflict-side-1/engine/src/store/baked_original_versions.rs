//! Metadata-only observation of the SAME original captured object versions.
//! This is not a full payload SHA proof, Root grant or renewed constructor.
use super::*;
pub struct BakedVersionSweep{source:Arc<BakedImageCas>,original:Arc<dyn ImageImportFence>,pending:Option<File>,
    cursor:usize,attempted:bool,ready:bool,failure:Option<anyhow::Error>}
impl BakedImageCas{
    pub fn check_original_versions_retaining(self:&Arc<Self>,out:&mut Option<BakedVersionSweep>,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original baked version sweep occupied");
        *out=Some(BakedVersionSweep{source:self.clone(),original,pending:None,cursor:0,attempted:false,ready:false,failure:None});
        out.as_mut().unwrap().run()
    }
}
impl BakedVersionSweep{
    pub fn acknowledged_data(&self)->bool{self.ready}
    pub fn checked_objects_data(&self)->usize{self.cursor}
    pub fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    #[cfg(test)]pub(super) fn retains_pending_file_data(&self)->bool{self.pending.is_some()}
    fn run(&mut self)->Result<()> {
        ensure!(!self.attempted,"original version sweep cannot replay");self.attempted=true;
        if let Err(e)=self.run_inner(){self.failure=Some(e);bail!("original object version observation failed; retain carrier")}Ok(())
    }
    fn run_inner(&mut self)->Result<()> {
        let source=self.source.clone();let mut state=source.lock(&*self.original)?;
        let result=(||->Result<()>{ensure!(state.captured&&!state.failed&&state.inventory.as_ref().is_some_and(|d|d.objects.len()==state.object_versions.len()),"original version capture incomplete/fenced");
            source.check(&state,None,&*self.original)?;
            for(reference,expected)in &state.object_versions{
                self.original.remaining()?;let shard=&reference.digest[..2];let leaf=&reference.digest[2..];source.check(&state,Some(shard),&*self.original)?;
                ensure!(self.pending.is_none(),"original version leaf pending");self.pending=Some(source.physical(open_at(&state.shards[shard].file,leaf,false))?);
                let maximum=if state.inventory.as_ref().unwrap().roots.manifest==*reference{MANIFEST}else{CHUNK};
                source.physical((||->Result<()>{ensure!(regular(self.pending.as_ref().unwrap(),source.owner,maximum)?==*expected&&expected.len==reference.size_bytes,
                    "original baked object version changed");
                ensure!(stamp(&open_at(&state.shards[shard].file,leaf,false)?)?==*expected,"original baked object name/version changed");Ok(())})())?;
                self.original.remaining()?;self.cursor+=1;self.pending.take();
            }
            source.check(&state,None,&*self.original)?;self.original.remaining()?;self.ready=true;Ok(())})();
        // A cancelled/expired job keeps its original output but does not poison
        // immutable custody for an unrelated legitimate job. Physical failure
        // is absorbing, exactly as for an actual byte read.
        if source.physical_failed.load(Ordering::Acquire){state.failed=true;}result
    }
}
