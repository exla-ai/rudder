//! Source-dependency ACK for an ORIGINAL failed fork whose alternate-install
//! permit was never claimed. Not a usable repository or native drain proof.
use super::*;

pub(super) struct NeverLinkedRequest {
    original: FinalizationBudget,
    reconciles: u8,
}

/// Private target side of an interrupted metadata copy. Its actual source
/// File/Dir descriptors are deliberately excluded from the released proof.
struct RetainedPrivateCopy {
    output: File, parent: Arc<Dir>, temporary: OsString, final_name: OsString,
    expected: SeedFile, mode: u32, stage: CopyStage,
    observed: SeedFile,
}
impl RetainedPrivateCopy {
    fn check_current(&self,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        tick(budget,cancelled)?;self.parent.check()?;
        let observed=descriptor(&self.output,ForkLocalizationLimits::default(),&mut Work::default(),budget,cancelled)?;
        ensure!(observed==self.observed,"original partial private copy bytes changed");
        // These names/stage/expected bytes are retained recovery DATA. No
        // name is reopened or interpreted as a completed/accepted history.
        let _retained=(&self.temporary,&self.final_name,&self.expected,self.mode,self.stage);
        Ok(())
    }
}
fn check_private_descriptors(private:&Private,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
    tick(budget,cancelled)?;private.parent.check()?;
    for dir in private.dirs.values(){tick(budget,cancelled)?;dir.check()?;}
    if let Some(root)=private.dirs.get(Path::new("")) {
        // Original parent/name -> original captured root, using fstatat only.
        // No later directory adoption or pathname-absence certification.
        let name=c_name(&private.name)?;let mut stat=std::mem::MaybeUninit::<libc::stat>::uninit();
        ensure!(unsafe{libc::fstatat(private.parent.file.as_raw_fd(),name.as_ptr(),stat.as_mut_ptr(),libc::AT_SYMLINK_NOFOLLOW)}==0,
            "original private fork name changed");let stat=unsafe{stat.assume_init()};
        ensure!(identity(&root.file)?==(stat.st_dev as u64,stat.st_ino as u64)
            &&stat.st_mode&libc::S_IFMT==libc::S_IFDIR,"original private root identity changed");
    }
    // If root capture itself failed, the original parent + creation name and
    // every captured descriptor remain held. We do not certify its absence,
    // contents or usability. The positive never-attempted permit is the fact.
    Ok(())
}

/// Opaque, non-Serde, non-Clone acknowledgment of THIS original fork's source
/// dependency release. It cannot be converted into LocalizedMetadataFork.
/// All private partial/history custody remains retained; no shared FD survives.
#[must_use="retain original never-linked proof with native quiescence/guard through Root retirement ACK"]
pub struct NeverLinkedMetadataFork {
    private: Private,
    pending: Option<RetainedPrivateCopy>,
    _never_attempted: NeverAttemptedAlternateInstall,
    binding: AttemptBinding,
    seed_manifest: ObjectRef,
    shared_path: PathBuf,
}
impl NeverLinkedMetadataFork {
    pub fn binding(&self)->&AttemptBinding{&self.binding}
    pub fn seed(&self)->&ObjectRef{&self.binding.seed}
    pub fn seed_manifest(&self)->&ObjectRef{&self.seed_manifest}
    pub fn manifest(&self)->&ObjectRef{&self.seed_manifest}
    /// DATA only; never opened to check/reconstruct the released source.
    pub fn original_shared_objects_root(&self)->&Path{&self.shared_path}
    pub fn check_current(&self,budget:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        check_private_descriptors(&self.private,budget,cancelled)?;
        if let Some(copy)=&self.pending{copy.check_current(budget,cancelled)?;}
        tick(budget,cancelled)
    }
}
impl PrivateMetadataForkOwner {
    fn require_original_never_linked(&self)->Result<()> {
        ensure!(self.alternate_install_permit.is_some() && self.alternate_install_intent.is_none(),
            "original alternate install permit was claimed; never-linked proof forbidden");
        ensure!(!self.ready && !self.localized && !self.never_linked_released && self.localization.is_none()
            && self.private.is_some() && self.source.is_some(),"original partial fork custody unavailable");
        Ok(())
    }
    /// OFF App. Native callback must validate genuine never-started helper/
    /// enrollment gates AND actual original factory/preparation/TLS joins.
    /// No missing Child/flag/path is a quiescence proof supplied by this crate.
    pub fn release_never_linked_retaining(&mut self,out:&mut Option<NeverLinkedMetadataFork>,
        original:&FinalizationBudget,cancelled:&AtomicBool,
        mut check_never_started:impl FnMut(&AttemptBinding)->Result<()>)->Result<()> {
        ensure!(out.is_none() && self.never_linked_release.is_none(),"never-linked release already attempted/output occupied");
        self.require_original_never_linked()?;
        // Store the ORIGINAL request and unresolved state before any callback,
        // wait/IO or user-code panic. No retry creates a fresh cleanup budget.
        self.never_linked_release=Some(NeverLinkedRequest{original:original.clone(),reconciles:0});
        self.failed=true;
        self.finish_never_linked(out,cancelled,&mut check_never_started)
    }
    pub fn reconcile_never_linked_retaining(&mut self,out:&mut Option<NeverLinkedMetadataFork>,
        original:&FinalizationBudget,cancelled:&AtomicBool,
        mut check_never_started:impl FnMut(&AttemptBinding)->Result<()>)->Result<()> {
        ensure!(out.is_none() && !self.never_linked_released,"original completed ACK must be retained, never reissued");
        self.require_original_never_linked()?;
        let request=self.never_linked_release.as_mut().context("no original never-linked request to reconcile")?;
        ensure!(request.original.constraint()==original.constraint() && request.reconciles<2,
            "never-linked reconciliation cannot renew original deadline/attempt bound");
        request.reconciles+=1;
        self.finish_never_linked(out,cancelled,&mut check_never_started)
    }
    fn finish_never_linked(&mut self,out:&mut Option<NeverLinkedMetadataFork>,cancelled:&AtomicBool,
        check:&mut impl FnMut(&AttemptBinding)->Result<()>)->Result<()> {
        self.require_original_never_linked()?;
        let original=self.never_linked_release.as_ref().context("original never-linked request absent")?.original.clone();
        phase(&original,cancelled,&self.binding,check)?;
        check_private_descriptors(self.private.as_ref().unwrap(),&original,cancelled)?;
        let observed=if let Some(copy)=&self.pending {
            copy.parent.check()?;
            Some(descriptor(&copy.output,ForkLocalizationLimits::default(),&mut Work::default(),&original,cancelled)?)
        }else{None};
        phase(&original,cancelled,&self.binding,check)?;
        self.require_original_never_linked()?;
        // No fallible check/callback follows until the genuine receipt is in
        // caller storage. Move the original positive permit, never a bool.
        let permit=self.alternate_install_permit.take().unwrap();
        let private=self.private.take().unwrap();
        let pending=self.pending.take().map(|copy| {
            let CopyJob{source,output,parent,temporary,final_name,expected,mode,stage}=copy;
            drop(source); // actual original metadata source File/Dir FDs
            RetainedPrivateCopy{output,parent,temporary,final_name,expected,mode,stage,observed:observed.unwrap()}
        });
        drop(self.source.take()); // actual original metadata/shared root FDs
        *out=Some(NeverLinkedMetadataFork{private,pending,_never_attempted:permit,
            binding:self.binding.clone(),seed_manifest:self.seed_manifest.clone(),shared_path:self.shared_path.clone()});
        self.never_linked_released=true;self.failed=true;self.ready=false;
        #[cfg(test)]
        if take_factory_fault(FactoryFault::FenceAfterReleaseAck){original.cancel();}
        out.as_ref().unwrap().check_current(&original,cancelled)
    }
}

#[cfg(test)]
#[derive(Clone,Copy,PartialEq,Eq)]
pub(super) enum FactoryFault { AfterOwnerStaged, AfterAlternateIntent, AfterAlternateInstalled, FenceAfterReleaseAck }
#[cfg(test)]
std::thread_local!{static FACTORY_FAULT:std::cell::Cell<Option<FactoryFault>>=const{std::cell::Cell::new(None)};}
#[cfg(test)]
fn take_factory_fault(at:FactoryFault)->bool{FACTORY_FAULT.with(|slot|{if slot.get()==Some(at){slot.set(None);true}else{false}})}
#[cfg(test)]
pub(super) fn factory_fault(at:FactoryFault)->Result<()> {
    ensure!(!take_factory_fault(at),"injected original fork creation failure");Ok(())
}
#[cfg(test)]
#[path="seed_fork_never_linked_tests.rs"]
mod tests;
