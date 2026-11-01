//! Successor immutable bytes from an actually verified, already published jj
//! operation. Native retains its owner/Journal/acceptance lock and gates every
//! phase. This library builder supplies no acceptance, lease or task authority.
use super::*;
use crate::seed::{RetainedSeedCasCache,SeedCasGeneration};
use std::sync::atomic::{AtomicBool,Ordering};

/// Stage before pin/CAS effects. Failure keeps original repo/verifier/cache,
/// partial seed, pin intent and actual prepared result for native recovery.
pub struct AcceptedSourceGeneration {
    repo:Arc<ReadonlyRepo>,verified:Arc<VerifiedSourceProjection>,cache:Arc<RetainedSeedCasCache>,
    store:LocalStore,source:ObjectRef,runnable:ObjectRef,environment:ObjectRef,
    previous:CommitIdentity,previous_seed:ObjectRef,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    pin:String,seed_pin:String,pin_attempted:bool,pin_ack:bool,seed:Option<SeedCasGeneration>,paged:Option<crate::seed::PagedSeedGeneration>,
    prepared:Option<PreparedInitialSource>,attempted:bool,acknowledged:bool,
}
impl AcceptedSourceGeneration {
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_retaining(out:&mut Option<Self>,store:LocalStore,repo:Arc<ReadonlyRepo>,
        verified:Arc<VerifiedSourceProjection>,cache:Arc<RetainedSeedCasCache>,source:ObjectRef,
        runnable:ObjectRef,environment:ObjectRef,previous:CommitIdentity,previous_seed:ObjectRef,
        pin:String,seed_pin:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>,mut check_owner:impl FnMut()->Result<()>)->Result<()> {
        ensure!(out.is_none(),"original accepted source generation occupied");
        *out=Some(Self{store,repo,verified,cache,source,runnable,environment,previous,previous_seed,original,cancelled,
            pin,seed_pin,pin_attempted:false,pin_ack:false,seed:None,paged:None,prepared:None,attempted:false,acknowledged:false});
        out.as_mut().unwrap().prepare(&mut check_owner)
    }
    fn time(&self)->Result<()> {if self.cancelled.load(Ordering::Acquire){self.original.cancel();anyhow::bail!("accepted source generation cancelled");}self.original.remaining()?;Ok(())}
    fn validate(&self,check_owner:&mut impl FnMut()->Result<()>)->Result<CommitIdentity> {
        self.phase(check_owner)?;self.previous.validate()?;self.cache.check_current()?;
        let head=self.repo.view().get_local_bookmark(RefName::new(INTEGRATION_BOOKMARK)).as_normal().context("accepted source requires exact integration bookmark")?;
        let commit=self.repo.store().get_commit(head)?;let baseline=CommitIdentity::from_jj(&commit);
        ensure!(self.repo.view().get_wc_commit_id(WorkspaceName::DEFAULT)==Some(commit.id())
            && commit.parent_ids()==&[CommitId::new(hex::decode(&self.previous.commit.0)?)]
            && self.repo.index().is_ancestor(&CommitId::new(hex::decode(&self.previous.commit.0)?),commit.id())?,
            "accepted generation loses original parent history or WC baseline");
        let runnable= rudder_engine::snapshot::decode_runnable(&self.store.read_object(&self.runnable)?)?;
        let source= rudder_engine::snapshot::decode_source(&self.store.read_object(&self.source)?)?;
        ensure!(runnable.source==self.source && runnable.environment.as_ref()==Some(&self.environment)
            && runnable.view_root==source.directory && source.consistency==CheckpointConsistency::Quiesced
            && self.verified.receipt().source==self.source && self.verified.receipt().tree==baseline.tree
            && self.verified.receipt().projection_policy==source.projection_policy,
            "accepted generation differs from actual retained mapping/source/environment");
        let pins=self.store.retention()?;
        for root in [&self.runnable,&self.environment,self.verified.evidence(),&self.previous_seed] {
            ensure!(pins.pins.values().any(|r|r==root),"accepted source input lost original retention");
        }
        self.phase(check_owner)?;Ok(baseline)
    }
    fn phase(&self,check_owner:&mut impl FnMut()->Result<()>)->Result<()> {self.time()?;check_owner()?;self.time()}
    fn prepare(&mut self,check_owner:&mut impl FnMut()->Result<()>)->Result<()> {
        ensure!(!self.attempted,"accepted generation cannot replay");self.attempted=true;
        let baseline=self.validate(check_owner)?;let operation=JjOperationId(self.repo.op_id().hex());
        ensure!(!self.store.retention()?.pins.contains_key(&self.pin),"accepted source original pin already exists");
        self.phase(check_owner)?;self.pin_attempted=true;self.store.pin_existing(&self.pin,&self.runnable)?;self.pin_ack=true;self.phase(check_owner)?;
        let seed_root=match self.cache.profile(){
            crate::seed::SeedProfile::SmallV1=>{SeedCasGeneration::prepare_retaining(&mut self.seed,self.cache.clone(),baseline.clone(),operation.clone(),self.seed_pin.clone(),
                &self.previous_seed,self.original.clone(),self.cancelled.clone(),check_owner)?;self.seed.as_ref().unwrap().root()?.clone()}
            crate::seed::SeedProfile::PagedCowV2=>{crate::seed::PagedSeedGeneration::prepare_retaining(&mut self.paged,self.cache.clone(),baseline.clone(),operation.clone(),self.seed_pin.clone(),
                Some(&self.previous_seed),self.original.clone(),self.cancelled.clone(),check_owner)?;self.paged.as_ref().unwrap().root()?.clone()}
        };self.validate(check_owner)?;
        self.prepared=Some(PreparedInitialSource {_workspace:None,repository:self.repo.clone(),baseline,operation,
            runnable_root:self.runnable.clone(),source_root:self.source.clone(),seed_root,runnable_pin:self.pin.clone(),
            seed_pin:self.seed_pin.clone(),verified:self.verified.clone(),seed_cache:self.cache.clone()});
        self.phase(check_owner)?;self.acknowledged=true;Ok(())
    }
    /// Original output is stored before native final checks. Calling this late
    /// cannot renew the job cutoff or turn an unacknowledged pin into readiness.
    pub fn take_prepared_retaining(&mut self,out:&mut Option<PreparedInitialSource>)->Result<()> {
        self.time()?;ensure!(self.acknowledged&&self.pin_ack&&out.is_none(),"accepted generation original ACK/output absent");
        self.validate(&mut ||Ok(()))?;if let Some(seed)=&self.paged{seed.check_captured()?;}else{self.seed.as_ref().context("accepted seed owner absent")?.check_captured()?;}
        *out=self.prepared.take();ensure!(out.is_some(),"accepted generation already transferred");self.time()
    }
    pub fn seed_work(&self)->Option<crate::seed::SeedCasWork>{self.seed.as_ref().map(SeedCasGeneration::work).or_else(||self.paged.as_ref().map(|p|{let w=p.work();crate::seed::SeedCasWork{read_files:w.read_files,read_bytes:w.read_bytes,reused_files:w.reused_files}}))}
}

#[cfg(test)]
#[path="accepted_source_generation_tests.rs"]
mod tests;
