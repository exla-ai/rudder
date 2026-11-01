//! Runtime-only environment preparation from an ACTUAL Root runtime client.
//! No compiler SDK, project/lease, task or default activation. Its consumer
//! retains this actual variant through source/export instead of a local pin.
use super::*;
use crate::managed_machine_cas::ManagedMachineCas;
use rudder_native::root_storage_bootstrap::runtime_image::{RuntimeImageClient,
    environment::{RuntimeBackingTransfer,RuntimeEnvironmentBacking}};
use rudder_engine::{build::{BuildIndex,PreparationSpec,PreparedContentLayout,PlatformIdentity,ToolchainIdentity,
        RetainedBackedPreparation,BackedPreparationAcknowledgment},
    store::{ImageImportFence,LocalImageImport,LocalImagePinAck,backed_retention::BackedRead,composed::ComposedImageStore},
    content::ContentStore,directory::{DirectoryEntry as PagedEntry,EntryNode as PagedNode,PagedDirectory}};
use std::time::Duration;

struct Fence {original:FinalizationBudget,cancelled:Arc<AtomicBool>,machine:Arc<ManagedMachineCas>,backing:Arc<RuntimeEnvironmentBacking>}
impl ImageImportFence for Fence {fn remaining(&self)->Result<Duration>{
    clock(&self.original,&self.cancelled)?;self.machine.assert_custody()?;
    // Nested storage reads invoke this while holding their actual Baked mutex.
    // Never recurse into that reader here. The original packet/client custody
    // is independent; full backing checks remain at unlocked phase boundaries.
    self.backing.assert_original_custody()?;
    self.original.remaining()
}}
struct ClockOnly {original:FinalizationBudget,cancelled:Arc<AtomicBool>}
impl ImageImportFence for ClockOnly {fn remaining(&self)->Result<Duration>{clock(&self.original,&self.cancelled)?;self.original.remaining()}}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum RuntimeEnvironmentPhase {Captured,WaitingBacking,Preparing,Ready,Failed}
impl ManagedHostRuntime {
    /// Explicit alternative producer, not a legacy host-import fallback or a
    /// conversion to HostRuntime. Only the typed source/export consumer can
    /// retain this distinct backed variant; legacy task callers stay guarded.
    pub(crate) fn capture_runtime_backed_environment_retaining(out:&mut Option<ManagedRuntimeEnvironmentFactory>,
        machine:Arc<ManagedMachineCas>,client:Arc<RuntimeImageClient>,index:Arc<BuildIndex>,
        original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ManagedRuntimeEnvironmentFactory::capture_retaining(out,machine,client,index,original,cancelled)
    }
}
/// Stage this owner before the first service request. The service/client keep
/// originals too. Known lane contention retains this SAME original deadline.
/// Any error/panic keeps request, packet, backing, import and preparation cells.
pub(crate) struct ManagedRuntimeEnvironmentFactory {
    machine:Arc<ManagedMachineCas>,client:Arc<RuntimeImageClient>,index:Arc<BuildIndex>,
    original:FinalizationBudget,cancelled:Arc<AtomicBool>,phase:RuntimeEnvironmentPhase,
    transfer:Option<RuntimeBackingTransfer>,backing:Option<Arc<RuntimeEnvironmentBacking>>,
    environment:Option<ManagedRuntimeBackedEnvironment>,failure:Option<anyhow::Error>,preparing:bool,
}
impl ManagedRuntimeEnvironmentFactory {
    pub(crate) fn capture_retaining(out:&mut Option<Self>,machine:Arc<ManagedMachineCas>,client:Arc<RuntimeImageClient>,
        index:Arc<BuildIndex>,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"original runtime environment factory already staged");
        *out=Some(Self{machine,client,index,original,cancelled,phase:RuntimeEnvironmentPhase::Captured,
            transfer:None,backing:None,environment:None,failure:None,preparing:false});
        let owner=out.as_mut().unwrap();match owner.check_initial(){Ok(())=>Ok(()),Err(e)=>{
            owner.phase=RuntimeEnvironmentPhase::Failed;owner.failure=Some(e);anyhow::bail!("original runtime environment capture failed; retain owner")}}
    }
    fn check_initial(&self)->Result<()> {
        clock(&self.original,&self.cancelled)?;
        self.client.assert_environment_scope(self.machine.config(),self.machine.registration())?;
        self.machine.assert_custody()?;
        ensure!(self.index.store().root()==self.machine.store()?.root(),"runtime preparation index uses another actual machine CAS");
        ensure!(cfg!(target_env="gnu"),"runtime backing preparation supports only the qualified Linux GNU image profile");Ok(())
    }
    pub(crate) fn poll_retaining(&mut self)->Result<bool> {
        ensure!(self.phase!=RuntimeEnvironmentPhase::Failed&&!self.preparing,"original runtime environment failed/interrupted; no replay");
        if self.phase==RuntimeEnvironmentPhase::Ready{return Ok(true)}
        match self.poll_inner(){Ok(v)=>Ok(v),Err(e)=>{self.phase=RuntimeEnvironmentPhase::Failed;self.failure=Some(e);anyhow::bail!("original runtime environment preparation failed; retain whole factory")}}
    }
    fn poll_inner(&mut self)->Result<bool> {
        self.check_initial()?;
        if self.transfer.is_none(){
            if !self.client.try_begin_environment_backing_retaining(&mut self.transfer,self.original.clone(),self.cancelled.clone())?{return Ok(false)}
            self.phase=RuntimeEnvironmentPhase::WaitingBacking;
        }
        if self.backing.is_none()&&!self.transfer.as_mut().unwrap().take_backing_retaining(&mut self.backing)?{return Ok(false)}
        let backing=self.backing.as_ref().unwrap().clone();
        ensure!(Arc::ptr_eq(backing.registration(),self.machine.registration())&&Arc::ptr_eq(backing.config(),self.machine.config()),
            "actual runtime backing belongs to another original machine config/registration");
        self.phase=RuntimeEnvironmentPhase::Preparing;self.preparing=true;
        self.environment=Some(ManagedRuntimeBackedEnvironment::stage(self.machine.clone(),backing,self.index.clone(),self.original.clone(),self.cancelled.clone()));
        self.environment.as_mut().unwrap().prepare_retaining()?;
        // Actual environment/ACK is retained BEFORE final fallible checks.
        self.check_initial()?;self.environment.as_ref().unwrap().assert_custody(&self.original,&self.cancelled)?;
        self.preparing=false;self.phase=RuntimeEnvironmentPhase::Ready;Ok(true)
    }
    pub(crate) fn phase_data(&self)->RuntimeEnvironmentPhase{self.phase}
    pub(crate) fn failure(&self)->Option<&anyhow::Error>{self.failure.as_ref()}
    pub(crate) fn environment(&self)->Result<&ManagedRuntimeBackedEnvironment>{
        ensure!(self.phase==RuntimeEnvironmentPhase::Ready,"original runtime environment ACK absent/unknown");self.environment.as_ref().context("actual runtime environment owner absent")
    }
}

/// This complete original owner is the backed-environment variant. It does
/// not implement or convert to the legacy host-import environment owner.
pub(crate) struct ManagedRuntimeBackedEnvironment {
    machine:Arc<ManagedMachineCas>,backing:Arc<RuntimeEnvironmentBacking>,index:Arc<BuildIndex>,
    fence:Arc<Fence>,network:NetworkInputs,composed:Option<Arc<ComposedImageStore>>,
    import:Option<LocalImageImport>,network_ack:Option<LocalImagePinAck>,network_root:Option<ObjectRef>,network_manifest:Option<ObjectRef>,
    preparation:Option<RetainedBackedPreparation>,post_read:Option<BackedRead>,spec:Option<PreparationSpec>,attempted:bool,ready:bool,nonce:String,
}
impl ManagedRuntimeBackedEnvironment {
    fn stage(machine:Arc<ManagedMachineCas>,backing:Arc<RuntimeEnvironmentBacking>,index:Arc<BuildIndex>,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Self {
        let fence=Arc::new(Fence{original,cancelled,machine:machine.clone(),backing:backing.clone()});
        Self{machine,backing,index,fence,network:NetworkInputs::staged(),composed:None,import:None,network_ack:None,
            network_root:None,network_manifest:None,preparation:None,post_read:None,spec:None,attempted:false,ready:false,nonce:uuid::Uuid::new_v4().to_string()}
    }
    fn prepare_retaining(&mut self)->Result<()> {
        ensure!(!self.attempted,"original runtime environment already attempted");self.attempted=true;self.fence.remaining()?;
        ComposedImageStore::capture_retaining(&mut self.composed,self.machine.store()?.clone(),self.backing.baked().clone(),self.fence.clone())?;
        self.network.capture_retaining(&self.fence.original,&self.fence.cancelled)?;
        LocalImageImport::stage_retaining(&mut self.import,self.machine.store()?.clone(),self.fence.clone())?;
        let import=self.import.as_ref().unwrap();let mut entries=Vec::new();let mut evidence=Vec::new();
        for input in &self.network.files {self.fence.remaining()?;input.current()?;
            let blob=ContentStore::put_blob(import,&input.bytes)?;
            let name=input.path.file_name().context("actual network basename absent")?.as_encoded_bytes();
            entries.push(PagedEntry::new(name,PagedNode::File{object:blob.clone(),logical_size:blob.size_bytes,mode:0o444})?);
            evidence.push(serde_json::json!({"guest":input.path,"content":blob}));
        }
        self.network_root=Some(PagedDirectory::new(import).build(0o555,entries)?);
        let bytes=canonical_json(&serde_json::json!({"format_version":1,"scope":"original-machine-public-network-v1","files":evidence}))?;
        let manifest=ObjectRef::from_bytes(ObjectKind::Blob,&bytes);self.network_manifest=Some(manifest.clone());
        import.receive_manifest(&manifest,&bytes)?;
        self.network_ack=Some(import.commit_pins(&format!("runtime-network/{}/tree",self.nonce),self.network_root.as_ref().unwrap(),
            &format!("runtime-network/{}/manifest",self.nonce),&manifest)?);
        import.assert_pins_current_for_import(self.network_ack.as_ref().unwrap())?;self.fence.remaining()?;
        let inputs=BTreeMap::from([("image/installer".into(),self.backing.installer_input().clone()),("image/manifest".into(),self.backing.manifest().clone()),
            ("vm/network".into(),self.network_root.as_ref().unwrap().clone()),("vm/network-manifest".into(),manifest.clone())]);
        let tools=self.backing.runtime_tools_data();ensure!(tools.len()==4,"actual runtime tool proof incomplete");
        // These are the four actual runtime ELFs already checked by Root.
        // There is deliberately no rustc/tsc/buck2/SDK identity or grant.
        let toolchains=tools.iter().map(|tool|(tool.name.clone(),ToolchainIdentity{version:format!("sha256:{}",tool.sha256),
            closure:self.backing.installer_input().clone(),executable:tool.content.clone(),executable_path:tool.installer_path.clone(),
            configuration:BTreeMap::from([("role".into(),"runtime-only".into())])})).collect();
        let config=self.backing.config();
        self.spec=Some(PreparationSpec{format_version:1,base_runtime:self.backing.installer_input().clone(),
            // Custody/registration/image attestation is checked by the retained
            // backing, not mixed into semantic content identity. Actual image,
            // tools and public VM inputs are already explicit content roots.
            platform:runtime_platform(&config.data().runtime_manifest.profile_id),
            installer_inputs:inputs,installer_commands:Vec::new(),installer_environment:BTreeMap::new(),toolchains,
            policy_version:"root-runtime-backed-environment-v1".into(),effects:BTreeSet::new()});
        self.index.prepare_backed_retaining(&mut self.preparation,self.spec.as_ref().unwrap().clone(),PreparedContentLayout::PagedChunkedV2,
            self.backing.installer_input().clone(),vec![self.network_root.as_ref().unwrap().clone(),manifest],vec![self.backing.inputs().clone()],None,
            self.fence.original.clone(),self.fence.cancelled.clone())?;
        let ack=self.preparation.as_ref().unwrap().acknowledgment()?;
        ack.retention().read_retaining(&mut self.post_read,ack.prepared().root.clone(),self.fence.clone())?;
        let environment=rudder_engine::snapshot::decode_environment(self.post_read.as_ref().unwrap().verified_bytes()?)?;
        ensure!(environment.directory==*self.backing.installer_input()&&environment.installer_inputs.contains(self.network_root.as_ref().unwrap()),
            "actual backed environment lost runtime/network roots");
        self.fence.remaining()?;self.network.current()?;self.post_read.take();self.ready=true;Ok(())
    }
    pub(crate) fn assert_custody(&self,original:&FinalizationBudget,cancelled:&Arc<AtomicBool>)->Result<()> {
        clock(original,cancelled)?;ensure!(self.ready,"original runtime environment ACK absent");
        self.machine.assert_custody()?;self.backing.assert_current(&ClockOnly{original:original.clone(),cancelled:cancelled.clone()})?;
        self.network.current()?;self.preparation.as_ref().context("actual backed preparation owner absent")?.acknowledgment()?;
        clock(original,cancelled)
    }
    pub(crate) fn preparation_ack(&self)->Result<&BackedPreparationAcknowledgment>{
        ensure!(self.ready,"original runtime environment ACK absent");self.preparation.as_ref().context("original backed preparation absent")?.acknowledgment()
    }
    pub(crate) fn derive_environment_subgraph_retaining(&self,out:&mut Option<Arc<rudder_engine::store::environment_dependencies::RetainedEnvironmentSubgraph>>,original:Arc<dyn ImageImportFence>)->Result<()> {
        self.assert_original_custody()?;original.remaining()?;
        self.preparation.as_ref().context("actual backed preparation absent")?.derive_environment_subgraph_retaining(out,original.clone())?;
        self.assert_original_custody()?;original.remaining()?;Ok(())
    }
    pub(crate) fn backing(&self)->&Arc<RuntimeEnvironmentBacking>{&self.backing}
    pub(crate) fn composed(&self)->Result<&Arc<ComposedImageStore>>{ensure!(self.ready,"runtime environment incomplete");self.composed.as_ref().context("actual composed owner absent")}
    pub(crate) fn machine(&self)->&Arc<ManagedMachineCas>{&self.machine}
    pub(crate) fn network_mounts(&self)->Result<Vec<ReadOnlyRuntime>>{ensure!(self.ready,"original runtime environment ACK absent");self.network.current()?;Ok(NETWORK_PATHS.into_iter().map(|p|ReadOnlyRuntime{source:p.into(),guest_path:p.into()}).collect())}
    pub(crate) fn assert_original_custody(&self)->Result<()> {
        ensure!(self.ready,"original runtime environment ACK absent");self.machine.assert_custody()?;
        self.backing.assert_original_custody()?;self.network.current()?;
        self.preparation.as_ref().context("actual retained preparation absent")?.acknowledgment()?;Ok(())
    }
}

// The only dynamic host inputs are these same three declared public network
// files. Loader/certs/static OS/model/package bytes stay in the baked graph.
const NETWORK_PATHS:[&str;3]=["/etc/hosts","/etc/resolv.conf","/etc/nsswitch.conf"];
struct NetworkInputs {files:Vec<NetworkFile>,attempted:bool}
impl NetworkInputs {
    fn staged()->Self{Self{files:Vec::new(),attempted:false}}
    fn capture_retaining(&mut self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        ensure!(!self.attempted,"actual network capture already attempted");self.attempted=true;
        for path in NETWORK_PATHS {clock(original,cancelled)?;self.files.push(NetworkFile::staged(path.into()));
            self.files.last_mut().unwrap().capture_retaining(original,cancelled,(0,0))?;}
        self.current()
    }
    fn current(&self)->Result<()>{ensure!(self.files.len()==3,"original VM network input set incomplete");for file in &self.files{file.current()?;}Ok(())}
}
#[derive(Clone,PartialEq,Eq)]
struct NetworkStamp {dev:u64,ino:u64,len:u64,mode:u32,uid:u32,gid:u32,mtime:(i64,i64),ctime:(i64,i64)}
fn network_stamp(m:&fs::Metadata)->NetworkStamp{NetworkStamp{dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),uid:m.uid(),gid:m.gid(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())}}
struct NetworkParent {path:PathBuf,file:Option<File>,saved:Option<NetworkStamp>}
struct NetworkFile {path:PathBuf,file:Option<File>,saved:Option<NetworkStamp>,parents:Vec<NetworkParent>,bytes:Vec<u8>,ready:bool,attempted:bool}
impl NetworkFile {
    fn staged(path:PathBuf)->Self{Self{path,file:None,saved:None,parents:Vec::new(),bytes:Vec::new(),ready:false,attempted:false}}
    fn capture_retaining(&mut self,original:&FinalizationBudget,cancelled:&AtomicBool,owner:(u32,u32))->Result<()> {
        ensure!(!self.attempted,"original network read cannot retry");self.attempted=true;clock(original,cancelled)?;
        ensure!(self.path.is_absolute()&&self.path.components().count()<=32,"actual network path unsupported");
        for path in self.path.ancestors().skip(1){clock(original,cancelled)?;
            self.parents.push(NetworkParent{path:path.into(),file:None,saved:None});let parent=self.parents.last_mut().unwrap();
            parent.file=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?);
            parent.saved=Some(network_stamp(&parent.file.as_ref().unwrap().metadata()?));
            let s=parent.saved.as_ref().unwrap();ensure!(((s.uid,s.gid)==owner||(s.uid,s.gid)==(0,0))&&s.mode&0o022==0,"network ancestor not original protected owner");
        }
        self.file=Some(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(&self.path)?);
        let m=self.file.as_ref().unwrap().metadata()?;self.saved=Some(network_stamp(&m));
        ensure!(m.is_file()&&m.nlink()==1&&m.len()<=1024*1024&&m.uid()==owner.0&&m.gid()==owner.1&&m.mode()&0o022==0,"network file not declared protected bounded regular input");
        self.current_open()?;self.bytes.try_reserve_exact(usize::try_from(m.len())?)?;let mut chunk=vec![0;16*1024];let mut offset=0;
        loop{clock(original,cancelled)?;let n=self.file.as_ref().unwrap().read_at(&mut chunk,offset)?;if n==0{break;}
            ensure!(offset+n as u64<=m.len(),"original VM network input grew");self.bytes.extend_from_slice(&chunk[..n]);offset+=n as u64;}
        ensure!(offset==m.len(),"original network file ended early");self.current_open()?;clock(original,cancelled)?;self.ready=true;Ok(())
    }
    fn current_open(&self)->Result<()> {
        let expected=self.saved.as_ref().context("actual network file capture incomplete")?;
        ensure!(network_stamp(&self.file.as_ref().context("actual network FD absent")?.metadata()?)==*expected
            &&network_stamp(&fs::symlink_metadata(&self.path)?)==*expected,"actual original network file changed/replaced");
        for parent in &self.parents {let file=parent.file.as_ref().context("original network ancestor FD absent")?;
            let saved=parent.saved.as_ref().context("original network ancestor validation incomplete")?;
            let held=file.metadata()?;let named=fs::symlink_metadata(&parent.path)?;
            // Parent directory contents may legitimately change; held/name
            // inode/type/owner/mode must stay the same and non-writable by peers.
            for m in [held,named]{ensure!(m.is_dir()&&!m.file_type().is_symlink()&&(m.dev(),m.ino(),m.uid(),m.gid(),m.mode())==(saved.dev,saved.ino,saved.uid,saved.gid,saved.mode),"original network ancestor changed");}}
        Ok(())
    }
    fn current(&self)->Result<()>{ensure!(self.ready,"original network read incomplete/unknown");self.current_open()}
}

#[cfg(test)]#[path="managed_backed_environment_tests.rs"]mod tests;

fn runtime_platform(profile:&str)->PlatformIdentity{PlatformIdentity{os:"linux".into(),architecture:std::env::consts::ARCH.into(),abi:"gnu".into(),
    properties:BTreeMap::from([("runtime_profile".into(),profile.into())])}}
#[cfg(test)]#[path="managed_runtime_preparation_identity_tests.rs"]mod content_identity_tests;
