//! Actual V2 private metadata mount custody. Native must retain its ORIGINAL
//! authenticated Root CoW ACK beside this owner. These checks are physical
//! continuity only; a mount/path never supplies project or task authority.
use super::*;
pub(super) struct CowMount {nonce:String,mount_id:Option<u64>,base:ObjectRef,never_mutated:Option<CowNeverMutated>,quiescence:Option<Arc<CowQuiescence>>}
struct CowNeverMutated{_private:()}
struct CowQuiescence {binding:AttemptBinding,nonce:String,seed_manifest:ObjectRef,original:FinalizationBudget}
/// Born only after the original factory has a typed quiescence callback and
/// releases its own original namespace FDs. No Clone/Serde/raw-data constructor.
pub struct CowMetadataForkQuiesced {actual:Arc<CowQuiescence>}
impl CowMetadataForkQuiesced {
    pub fn binding(&self)->&AttemptBinding{&self.actual.binding}
    pub fn mount_nonce(&self)->&str{&self.actual.nonce}
    pub fn seed_manifest(&self)->&ObjectRef{&self.actual.seed_manifest}
    pub fn check_current(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{
        ensure!(self.actual.original.constraint()==original.constraint(),"CoW quiescence original budget changed");tick(original,cancelled)
    }
}
fn kernel_mount(root:&Path,nonce:&str)->Result<u64>{
    let mut found=None;let mut bytes=Vec::new();File::open("/proc/thread-self/mountinfo")?.take((1<<20)+1).read_to_end(&mut bytes)?;ensure!(bytes.len()<=1<<20,"CoW mountinfo exceeds bound");
    for line in std::str::from_utf8(&bytes)?.lines(){let(l,r)=line.split_once(" - ").context("CoW mountinfo incomplete")?;
        let l=l.split(' ').collect::<Vec<_>>();let r=r.split(' ').collect::<Vec<_>>();ensure!(l.len()>=6&&r.len()>=3,"CoW mountinfo invalid");
        if l[4]==root.to_str().context("CoW root not UTF8")?{ensure!(found.is_none()&&r[0]=="fuse.rudder-seed"&&r[1]==format!("rudder-seed-{nonce}")
            &&l[5].split(',').any(|f|f=="rw")&&["nosuid","nodev","noexec"].iter().all(|f|l[5].split(',').any(|v|v==*f)),"CoW original kernel mount differs");found=Some(l[0].parse()?);}}
    found.context("CoW actual Root mount absent")
}
/// Production caller obtains `destination/nonce/base` ONLY from the original
/// Root ACK and retains that opaque client owner. VCS also requires an actual
/// current FUSE mount in the inherited protected host namespace.
pub fn fork_private_metadata_cow_retaining(out:&mut Option<PrivateMetadataForkOwner>,template:&VerifiedMetadataTemplate,
    destination:&Path,nonce:&str,base:&ObjectRef,binding:AttemptBinding,activation:Option<&crate::LiveObservation>,
    original:&FinalizationBudget,cancelled:&AtomicBool)->Result<PrivateMetadataFork>{
    ensure!(out.is_none(),"CoW original fork owner occupied");
    // Stage the ORIGINAL carrier and a positive never-mutated permit BEFORE
    // every fallible check/read. A cancelled constructor still has a genuine
    // unused owner, rather than inferring no effect from an absent pathname.
    *out=Some(PrivateMetadataForkOwner{private:None,source:None,require_readonly_objects:true,
        manifest:template.manifest.clone(),binding:binding.clone(),seed_manifest:template.identity.clone(),shared_path:template.object_mount.clone(),
        alternates:None,ready:false,pending:None,alternate_install_permit:None,alternate_install_intent:None,
        never_linked_release:None,never_linked_released:false,localization:None,history:None,failed:false,shared_index:0,
        verifier:PathBuf::new(),detach_intent:false,detached:false,work:Work::default(),reconciles:0,fault:None,localized:false,
        cow:Some(CowMount{nonce:nonce.into(),mount_id:None,base:base.clone(),never_mutated:Some(CowNeverMutated{_private:()}),quiescence:None})});
    let owner=out.as_mut().unwrap();
    let result=(||{
        tick(original,cancelled)?;binding.validate()?;let descriptor=template.paged_descriptor().context("CoW fork requires actual V2 template")?;
        ensure!(template.retained_seed()==Some(&binding.seed)&&template.require_readonly&&base.kind==ObjectKind::Directory&&base.size_bytes<=4096
            &&binding.baseline==descriptor.baseline&&binding.baseline_operation==descriptor.operation,"CoW original template/binding differs");
        ensure!(uuid::Uuid::parse_str(nonce)?.to_string()==nonce&&destination.is_absolute()&&destination.file_name().is_some_and(|n|n=="private"),"CoW mount nonce/path invalid");
        let mount_id=kernel_mount(destination,nonce)?;owner.cow.as_mut().unwrap().mount_id=Some(mount_id);
        let parent_path=destination.parent().context("CoW parent absent")?;
        for p in parent_path.ancestors(){let m=fs::symlink_metadata(p)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"CoW ancestor is not protected Root custody");}
        let parent=Dir::open(parent_path,false)?;let root=Dir::open(destination,true)?;let name=destination.file_name().unwrap().to_os_string();
        let mut dirs=BTreeMap::new();dirs.insert(PathBuf::new(),root);owner.private=Some(Private{parent,name,dirs});
        // LIVE siblings read the SAME actual RO object mount/inodes. Only
        // private metadata and new objects occupy this CoW namespace.
        let metadata=Dir::open(&template.root.join("metadata"),false)?;let shared=Dir::open(&template.object_mount,false)?;
        require_readonly_mount(&fd_path(&shared.file))?;owner.source=Some(Source{metadata,objects:shared});
        let objects=owner.private()?.dir(Path::new(".git/objects"),false,original,cancelled)?;
        let info=objects.child(std::ffi::OsStr::new("info"),true)?;let alternate=info.open_file(std::ffi::OsStr::new("alternates"))?;
        let expected=format!("{}\n",template.object_mount.to_str().context("original alternate path not UTF8")?);
        ensure!(alternate.file.metadata()?.len()==expected.len()as u64&&expected.len()<=4096,"CoW original alternate length differs");
        let mut bytes=vec![0;expected.len()];alternate.file.read_exact_at(&mut bytes,0)?;
        ensure!(bytes==expected.as_bytes(),"CoW mount uses another shared object dependency");owner.alternates=Some(alternate);
        let working=owner.private()?.dir(Path::new(".jj/working_copy"),false,original,cancelled)?;
        let kind=working.open_file(std::ffi::OsStr::new("type"))?;ensure!(kind.file.metadata()?.len()<=128,"CoW working-copy type exceeds bound");
        let mut bytes=vec![0;kind.file.metadata()?.len() as usize];kind.file.read_exact_at(&mut bytes,0)?;
        owner.cow.as_mut().unwrap().never_mutated.take().context("original CoW mutation permit already consumed")?;
        if bytes==crate::working_copy::TYPE_NAME.as_bytes(){crate::working_copy::rebind_private_fork(&working.path,&binding,activation.context("CoW Rudder WC needs actual observation")?)?;}
        let receipt=PrivateMetadataFork{format_version:1,seed_manifest:template.identity.clone(),binding,
            metadata_files:descriptor.metadata_files,metadata_bytes:descriptor.metadata_bytes,kernel_readonly_objects:true};
        let _ready=owner.inline("rudder-metadata-ready.json",canonical_json(&receipt)?,original,cancelled)?;
        ensure!(kernel_mount(destination,nonce)?==mount_id,"CoW mount changed during original factory");tick(original,cancelled)?;owner.ready=true;Ok(receipt)
    })();if result.is_err(){owner.failed=true;}result
}
impl PrivateMetadataForkOwner {
    pub fn is_cow(&self)->bool{self.cow.is_some()}
    pub fn quiesce_cow_retaining(&mut self,out:&mut Option<CowMetadataForkQuiesced>,original:&FinalizationBudget,cancelled:&AtomicBool,
        mut check_quiescent:impl FnMut(&AttemptBinding)->Result<()>)->Result<()>{
        tick(original,cancelled)?;let cow=self.cow.as_ref().context("original CoW mount owner absent")?;
        if let Some(actual)=&cow.quiescence{ensure!(out.is_some(),"original CoW receipt moved/dropped; cannot reconstruct");return out.as_ref().unwrap().check_current(original,cancelled);}
        ensure!(out.is_none()&&self.pending.is_none(),"CoW factory IO still unsettled");
        let nonce=cow.nonce.clone();let mount_id=cow.mount_id;
        if let Some(private)=&self.private{let root=private.root()?.path.clone();ensure!(Some(kernel_mount(&root,&nonce)?)==mount_id,"original CoW mount replaced");}
        else{ensure!(cow.never_mutated.is_some(),"CoW mutation began without retained original directory; recovery held");}
        // Mark the original owner nonready BEFORE trusted native validation;
        // callback panic retains the real carrier with no new use permitted.
        self.ready=false;check_quiescent(&self.binding)?;tick(original,cancelled)?;
        let actual=Arc::new(CowQuiescence{binding:self.binding.clone(),nonce:self.cow.as_ref().unwrap().nonce.clone(),seed_manifest:self.seed_manifest.clone(),original:original.clone()});
        // All genuine private ODB/service/process owners have drained above.
        // Close ONLY this original factory's FD aliases before normal unmount.
        self.private=None;self.source=None;self.alternates=None;
        self.cow.as_mut().unwrap().quiescence=Some(actual.clone());*out=Some(CowMetadataForkQuiesced{actual});tick(original,cancelled)
    }
}

#[cfg(test)]
impl PrivateMetadataForkOwner {
    pub(crate) fn shares_manifest_allocation(&self,template:&VerifiedMetadataTemplate)->bool{Arc::ptr_eq(&self.manifest,&template.manifest)}
}
