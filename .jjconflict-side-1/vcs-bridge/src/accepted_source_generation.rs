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
    #[cfg(target_os="linux")] inputs:Option<Arc<rudder_engine::store::environment_dependencies::EnvironmentDependentPinOwner>>,
    #[cfg(target_os="linux")] publication:Option<rudder_engine::store::environment_dependencies::EnvironmentDependentPublication>,
    #[cfg(target_os="linux")] read:Option<rudder_engine::store::environment_dependencies::EnvironmentDependentRead>,
    #[cfg(target_os="linux")] stage:Option<String>,
    #[cfg(target_os="linux")] overlay:Option<Arc<rudder_engine::store::environment_dependencies::SourceOverlayPinOwner>>,
    #[cfg(target_os="linux")] overlay_read:Option<rudder_engine::store::environment_dependencies::SourceOverlayRead>,
    #[cfg(target_os="linux")] source_parent:Option<Arc<rudder_engine::store::environment_dependencies::RetainedSourceParent>>,
}
impl AcceptedSourceGeneration {
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_retaining(out:&mut Option<Self>,store:LocalStore,repo:Arc<ReadonlyRepo>,
        verified:Arc<VerifiedSourceProjection>,cache:Arc<RetainedSeedCasCache>,source:ObjectRef,
        runnable:ObjectRef,environment:ObjectRef,previous:CommitIdentity,previous_seed:ObjectRef,
        pin:String,seed_pin:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>,mut check_owner:impl FnMut()->Result<()>)->Result<()> {
        ensure!(out.is_none(),"original accepted source generation occupied");
        *out=Some(Self{store,repo,verified,cache,source,runnable,environment,previous,previous_seed,original,cancelled,
            pin,seed_pin,pin_attempted:false,pin_ack:false,seed:None,paged:None,prepared:None,attempted:false,acknowledged:false,
            #[cfg(target_os="linux")] inputs:None,#[cfg(target_os="linux")] publication:None,#[cfg(target_os="linux")] read:None, #[cfg(target_os="linux")] stage:None,
            #[cfg(target_os="linux")] overlay:None,#[cfg(target_os="linux")] overlay_read:None,#[cfg(target_os="linux")] source_parent:None});
        out.as_mut().unwrap().prepare(&mut check_owner)
    }
    #[cfg(target_os="linux")]
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_backed_retaining(out:&mut Option<Self>,store:LocalStore,repo:Arc<ReadonlyRepo>,verified:Arc<VerifiedSourceProjection>,cache:Arc<RetainedSeedCasCache>,
        source:ObjectRef,inputs:Arc<rudder_engine::store::environment_dependencies::EnvironmentDependentPinOwner>,previous:CommitIdentity,previous_seed:ObjectRef,
        pin:String,seed_pin:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>,mut check_owner:impl FnMut()->Result<()>)->Result<()> {
        ensure!(out.is_none(),"original backed accepted generation occupied");let runnable=inputs.root_data().clone();let environment=inputs.environment().root_data()?.clone();
        *out=Some(Self{store,repo,verified,cache,source,runnable,environment,previous,previous_seed,original,cancelled,pin,seed_pin,pin_attempted:false,pin_ack:false,
            seed:None,paged:None,prepared:None,attempted:false,acknowledged:false,inputs:Some(inputs),publication:None,read:None,stage:None,overlay:None,overlay_read:None,source_parent:None});out.as_mut().unwrap().prepare(&mut check_owner)
    }
    /// Explicit full flatten of an actual published overlay. The overlay stays
    /// owned through every fallible phase and is never used as a source parent.
    #[cfg(target_os="linux")]
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_overlay_retaining(out:&mut Option<Self>,store:LocalStore,repo:Arc<ReadonlyRepo>,verified:Arc<VerifiedSourceProjection>,cache:Arc<RetainedSeedCasCache>,
        source:ObjectRef,overlay:Arc<rudder_engine::store::environment_dependencies::SourceOverlayPinOwner>,previous:CommitIdentity,previous_seed:ObjectRef,
        pin:String,seed_pin:String,original:FinalizationBudget,cancelled:Arc<AtomicBool>,mut check_owner:impl FnMut()->Result<()>)->Result<()> {
        ensure!(out.is_none(),"original overlay accepted generation occupied");let runnable=overlay.root_data().clone();
        let environment=overlay.source_parent().original_owner().environment().root_data()?.clone();
        *out=Some(Self{store,repo,verified,cache,source,runnable,environment,previous,previous_seed,original,cancelled,pin,seed_pin,pin_attempted:false,pin_ack:false,
            seed:None,paged:None,prepared:None,attempted:false,acknowledged:false,inputs:None,publication:None,read:None,stage:None,overlay:Some(overlay),overlay_read:None,source_parent:None});
        out.as_mut().unwrap().prepare(&mut check_owner)
    }
    #[cfg(target_os="linux")]
    fn environment_owner(&self)->Option<&Arc<rudder_engine::store::environment_dependencies::RetainedEnvironmentSubgraph>> {
        self.inputs.as_ref().map(|owner|owner.environment()).or_else(||self.overlay.as_ref().map(|owner|owner.source_parent().original_owner().environment()))
    }
    #[cfg(target_os="linux")]
    fn check_backed(&mut self)->Result<()> {if let Some(owner)=&self.inputs{ensure!(self.read.is_none(),"original accepted input read unresolved");
        owner.read_retaining(&mut self.read,self.runnable.clone(),Arc::new(backed::Clock{original:self.original.clone(),cancelled:self.cancelled.clone()}))?;
        self.read.as_ref().unwrap().verified_bytes()?;self.time()?;self.read.take();}
        if let Some(owner)=&self.overlay{ensure!(self.overlay_read.is_none(),"original accepted overlay read unresolved");
            owner.read_retaining(&mut self.overlay_read,self.runnable.clone(),Arc::new(backed::Clock{original:self.original.clone(),cancelled:self.cancelled.clone()}))?;
            self.overlay_read.as_ref().unwrap().verified_bytes()?;self.time()?;self.overlay_read.take();}Ok(())}
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
            #[cfg(target_os="linux")] if let Some(owner)=&self.inputs{if root==&self.runnable||root==&self.environment{
                ensure!(owner.matches_record_data(&pins,owner.pin_name_data())&&owner.root_data()==&self.runnable&&owner.environment().root_data()?==&self.environment,"accepted input original dependency changed");continue;}}
            #[cfg(target_os="linux")] if let Some(owner)=&self.overlay{if root==&self.runnable||root==&self.environment{
                ensure!(owner.matches_record_data(&pins,owner.pin_name_data())&&owner.root_data()==&self.runnable
                    &&owner.source_parent().original_owner().environment().root_data()?==&self.environment,"accepted input original overlay changed");continue;}}
            ensure!(pins.pins.values().any(|r|r==root),"accepted source input lost original retention");
        }
        self.phase(check_owner)?;Ok(baseline)
    }
    fn phase(&self,check_owner:&mut impl FnMut()->Result<()>)->Result<()> {self.time()?;check_owner()?;self.time()}
    fn prepare(&mut self,check_owner:&mut impl FnMut()->Result<()>)->Result<()> {
        ensure!(!self.attempted,"accepted generation cannot replay");self.attempted=true;
        #[cfg(target_os="linux")] self.check_backed()?;
        let baseline=self.validate(check_owner)?;let operation=JjOperationId(self.repo.op_id().hex());
        ensure!(!self.store.retention()?.pins.contains_key(&self.pin),"accepted source original pin already exists");
        self.phase(check_owner)?;self.pin_attempted=true;
        #[cfg(target_os="linux")] if let Some(environment)=self.environment_owner().cloned(){
            self.stage=Some(self.store.begin_staging(Duration::from_secs(120))?.id);
            // This full publisher traverses/verifies the actual accepted source
            // graph. No overlay record or ordinary repin can mint the next parent.
            self.store.publish_environment_dependent_retaining(&mut self.publication,self.stage.as_ref().unwrap().clone(),self.pin.clone(),self.runnable.clone(),environment,
                Arc::new(backed::Clock{original:self.original.clone(),cancelled:self.cancelled.clone()}))?;
            self.pin_ack=true;self.check_backed()?;self.phase(check_owner)?;
            self.publication.as_ref().unwrap().acknowledgment()?.derive_source_parent_retaining(&mut self.source_parent,
                Arc::new(backed::Clock{original:self.original.clone(),cancelled:self.cancelled.clone()}))?;
        }else{self.store.pin_existing(&self.pin,&self.runnable)?;}
        #[cfg(not(target_os="linux"))] self.store.pin_existing(&self.pin,&self.runnable)?;
        self.pin_ack=true;self.phase(check_owner)?;
        let seed_root=match self.cache.profile(){
            crate::seed::SeedProfile::SmallV1=>{SeedCasGeneration::prepare_retaining(&mut self.seed,self.cache.clone(),baseline.clone(),operation.clone(),self.seed_pin.clone(),
                &self.previous_seed,self.original.clone(),self.cancelled.clone(),check_owner)?;self.seed.as_ref().unwrap().root()?.clone()}
            crate::seed::SeedProfile::PagedCowV2=>{crate::seed::PagedSeedGeneration::prepare_retaining(&mut self.paged,self.cache.clone(),baseline.clone(),operation.clone(),self.seed_pin.clone(),
                Some(&self.previous_seed),self.original.clone(),self.cancelled.clone(),check_owner)?;self.paged.as_ref().unwrap().root()?.clone()}
        };self.validate(check_owner)?;
        self.prepared=Some(PreparedInitialSource {_workspace:None,repository:self.repo.clone(),baseline,operation,
            runnable_root:self.runnable.clone(),source_root:self.source.clone(),seed_root,runnable_pin:self.pin.clone(),
            seed_pin:self.seed_pin.clone(),verified:self.verified.clone(),seed_cache:self.cache.clone(),
            #[cfg(target_os="linux")] runnable_backing:None,
            #[cfg(target_os="linux")] runnable_dependency:self.publication.as_ref().map(|p|p.acknowledgment().cloned()).transpose()?,
            #[cfg(target_os="linux")] source_parent:self.source_parent.clone()});
        self.phase(check_owner)?;self.acknowledged=true;Ok(())
    }
    /// Original output is stored before native final checks. Calling this late
    /// cannot renew the job cutoff or turn an unacknowledged pin into readiness.
    pub fn take_prepared_retaining(&mut self,out:&mut Option<PreparedInitialSource>)->Result<()> {
        self.time()?;ensure!(self.acknowledged&&self.pin_ack&&out.is_none(),"accepted generation original ACK/output absent");
        #[cfg(target_os="linux")] self.check_backed()?;
        self.validate(&mut ||Ok(()))?;if let Some(seed)=&self.paged{seed.check_captured()?;}else{self.seed.as_ref().context("accepted seed owner absent")?.check_captured()?;}
        *out=self.prepared.take();ensure!(out.is_some(),"accepted generation already transferred");self.time()
    }
    pub fn seed_work(&self)->Option<crate::seed::SeedCasWork>{self.seed.as_ref().map(SeedCasGeneration::work).or_else(||self.paged.as_ref().map(|p|{let w=p.work();crate::seed::SeedCasWork{read_files:w.read_files,read_bytes:w.read_bytes,reused_files:w.reused_files}}))}
}

#[cfg(test)]
#[path="accepted_source_generation_tests.rs"]
mod tests;
