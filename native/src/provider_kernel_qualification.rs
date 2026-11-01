//! One actual current-boot startup probe; no project/PG/model authority.
//! Always OFF App. No serialized report can construct the resulting owner.
#![cfg(target_os="linux")]
use crate::agent_broker::{AgentBroker,BrokerConfig,BrokerThreadOwner,BrokerThreadsSettled};
use anyhow::{ensure,Context,Result};
use rudder_engine::{agent_api::{AgentIdentityFile,now_ms},directory::{DirectoryEntry,EntryNode,LocalDirectoryStore,PagedDirectory},
    execution::{self,ExecutionSpec,ExecutionProfileV4,ExecutionWorkspaceSelectionV4,WorkspaceObservationProfile,ReadOnlyRuntime,BrokerBinding,ManagedExecutionBinding,ManagedFrontendStart,StopAndCheckpoint,
        finalization_budget::FinalizationBudget,handoff::{ManagedExecutionHandoff,RetainedManagedExecutionActivation,ManagedExecutionHandle,ManagedExecutionEvent,ValidatedManagedShutdown,BackendProcessWitness,HandoffReceipt,WorkerJoined},network::{NetworkProfileV1,NetworkBinding,NetworkEpoch,NetworkExecutionProfile,NetworkMode,NetworkBudgets}},
    journal::{Journal,CapabilityGrant,PreparedCapabilityIssuance},protocol::{OwnerEpoch,canonical_json},store::LocalStore,
    vfs::{VfsManager,ObservationBudget,exclusion_policy_digest,rpc::{ObservationBinding,AttemptContext}},workspace::WorkspaceProfile};
use rudder_native::{engine_registration::RegisteredEngineOwner,host_ingress::HostIngressConfigOwner};
use serde::{Serialize,Deserialize};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::{collections::{BTreeMap,BTreeSet},ffi::CStr,fs::{self,File,OpenOptions},io::{Read,Write},os::{fd::{AsRawFd,FromRawFd,OwnedFd},unix::{fs::{FileExt,MetadataExt,OpenOptionsExt,PermissionsExt},process::CommandExt}},path::{Path,PathBuf},process::{Child,Command,ExitStatus,Stdio},sync::{Arc,Mutex,atomic::{AtomicBool,AtomicPtr,Ordering}},time::Duration};
use uuid::Uuid;
const NATIVE:&str="/opt/rudder/dist/native/rudder-native";
const MODULES:&str=execution::provider_kernel::PROVIDER_KERNEL_DIRECTORY;
const PROFILE:&str="spec4-local-complete-offline-provider-kernel-empty-v1";
const GUEST:&str=include_str!("../tests/fixtures/provider_kernel/probe.py");
const LOG_LIMIT:usize=64*1024;
#[path="provider_kernel_retirement.rs"]
mod retirement;
pub(crate) use retirement::KernelProbeFixtureRetired;
static ONE_PROBE:AtomicBool=AtomicBool::new(false);
static RECOVERY:AtomicPtr<State>=AtomicPtr::new(std::ptr::null_mut());
fn parent_floor()->Result<()> {let cap=execution::network::linux::read_capabilities().map_err(anyhow::Error::new)?;ensure!(unsafe{libc::getuid()}==1500&&unsafe{libc::geteuid()}==1500&&unsafe{libc::getgid()}==1500&&unsafe{libc::getegid()}==1500&&unsafe{libc::getgroups(0,std::ptr::null_mut())}==0&&cap.no_new_privileges&&cap.all_five_zero(),"kernel probe requires original UID1500/all-five-zero/NNP parent");Ok(())}
fn fault()->anyhow::Error{anyhow::anyhow!("kernel startup probe failed; retain original owner for recovery")}
fn time(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {ensure!(!cancel.load(Ordering::Acquire),"kernel startup probe cancelled");original.remaining()?;Ok(())}
fn sha(bytes:&[u8])->String{format!("{:x}",Sha256::digest(bytes))}
#[derive(Clone,PartialEq,Eq)]
struct Stamp {dev:u64,ino:u64,len:u64,mode:u32,uid:u32,gid:u32,links:u64,mt:(i64,i64),ct:(i64,i64)}
fn stamp(m:&fs::Metadata)->Stamp{Stamp{dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),uid:m.uid(),gid:m.gid(),links:m.nlink(),mt:(m.mtime(),m.mtime_nsec()),ct:(m.ctime(),m.ctime_nsec())}}
fn private(path:&Path)->Result<()> {let m=fs::symlink_metadata(path)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==1500&&m.gid()==1500&&m.mode()&0o7777==0o700&&path.canonicalize()?==path,"kernel probe parent must be an actual private UID1500 directory");Ok(())}
fn mkdir(path:&Path)->Result<()> {fs::create_dir(path)?;fs::set_permissions(path,fs::Permissions::from_mode(0o700))?;private(path)}
fn bounded_hash(file:&File,bytes:u64,original:&FinalizationBudget,cancel:&AtomicBool)->Result<String> {let mut h=Sha256::new();let mut chunk=[0u8;65536];let mut at=0;
    while at<bytes{time(original,cancel)?;let limit=chunk.len().min((bytes-at)as usize);let n=file.read_at(&mut chunk[..limit],at)?;ensure!(n>0,"kernel probe file truncated");at+=n as u64;h.update(&chunk[..n]);}
    let mut end=[0u8;1];ensure!(file.read_at(&mut end,at)?==0,"kernel probe file grew");time(original,cancel)?;Ok(format!("{:x}",h.finalize()))}
struct Image {file:File,identity:Stamp,parents:Vec<(PathBuf,File,Stamp)>,sha:String}
impl Image {
    fn capture(expected:&str,original:&FinalizationBudget,cancel:&AtomicBool)->Result<Self> {
        ensure!(expected.len()==64&&expected.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)),"native image SHA invalid");let path=Path::new(NATIVE);let mut parents=Vec::new();
        for p in path.ancestors().skip(1){let m=fs::symlink_metadata(p)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"native image ancestor unprotected");let f=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(p)?;ensure!(stamp(&f.metadata()?)==stamp(&m),"native image ancestor open changed");parents.push((p.into(),f,stamp(&m)));}
        let m=fs::symlink_metadata(path)?;ensure!(m.is_file()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.nlink()==1&&m.mode()&0o7777==0o555&&m.len()>0&&m.len()<=512*1024*1024,"native image not exact protected ELF");
        let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(path)?;ensure!(stamp(&file.metadata()?)==stamp(&m),"native image open changed");let digest=bounded_hash(&file,m.len(),original,cancel)?;ensure!(digest==expected,"native image SHA differs");let value=Self{file,identity:stamp(&m),parents,sha:digest};value.current()?;Ok(value)
    }
    fn current(&self)->Result<()> {ensure!(stamp(&self.file.metadata()?)==self.identity&&stamp(&fs::symlink_metadata(NATIVE)?)==self.identity,"original native image changed");for(p,f,s)in &self.parents{ensure!(stamp(&f.metadata()?)==*s&&stamp(&fs::symlink_metadata(p)?)==*s,"original native image ancestor changed");}Ok(())}
}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]#[serde(deny_unknown_fields)]
struct NamespaceId {device:String,inode:String}
fn nsid(m:&fs::Metadata)->NamespaceId{NamespaceId{device:m.dev().to_string(),inode:m.ino().to_string()}}
#[derive(Clone,Serialize,PartialEq,Eq)]struct Kernel {system:String,release:String,version:String,machine:String}
fn kernel()->Result<Kernel>{let mut raw=std::mem::MaybeUninit::<libc::utsname>::uninit();ensure!(unsafe{libc::uname(raw.as_mut_ptr())}==0,"kernel uname failed");let raw=unsafe{raw.assume_init()};let text=|p:*const libc::c_char|->Result<String>{Ok(unsafe{CStr::from_ptr(p)}.to_str()?.to_owned())};Ok(Kernel{system:text(raw.sysname.as_ptr())?,release:text(raw.release.as_ptr())?,version:text(raw.version.as_ptr())?,machine:text(raw.machine.as_ptr())?})}
fn read_boot(file:&File)->Result<String>{let mut raw=[0u8;128];let n=file.read_at(&mut raw,0)?;ensure!(n>0&&n<raw.len(),"boot observation truncated");let s=std::str::from_utf8(&raw[..n])?.trim().to_owned();ensure!(Uuid::parse_str(&s)?.to_string()==s,"boot identity invalid");Ok(s)}
struct ModuleEntry {path:PathBuf,held:Option<File>,identity:Stamp,link:Option<PathBuf>,digest:Option<String>}
struct Host {pid:u32,boot:File,boot_identity:NamespaceId,boot_id:String,kernel:Kernel,ns:Vec<(String,File,NamespaceId)>,modules:Vec<ModuleEntry>,payload_sha:String,regular:usize}
impl Host {
    fn capture(original:&FinalizationBudget,cancel:&AtomicBool)->Result<Self>{
        let boot=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open("/proc/sys/kernel/random/boot_id")?;let boot_id=read_boot(&boot)?;let boot_identity=nsid(&boot.metadata()?);let mut ns=Vec::new();
        for name in ["user","mnt","pid","net","uts"]{let f=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC).open(format!("/proc/self/ns/{name}"))?;ns.push((name.into(),f.try_clone()?,nsid(&f.metadata()?)));}
        let mut value=Self{pid:std::process::id(),boot,boot_identity,boot_id,kernel:kernel()?,ns,modules:Vec::new(),payload_sha:String::new(),regular:0};let mut pending=vec![(PathBuf::from(MODULES),0usize),(PathBuf::from(execution::provider_kernel::PROVIDER_SWAP_TOOL),0usize)];let mut unique=BTreeMap::<(u64,u64),String>::new();let mut total=0u64;
        while let Some((path,depth))=pending.pop(){time(original,cancel)?;ensure!(depth<=32&&value.modules.len()<4096,"provider observation namespace bound");let m=fs::symlink_metadata(&path)?;ensure!(m.uid()==0&&m.gid()==0&&(m.file_type().is_symlink()||m.mode()&0o022==0),"provider object unprotected");let identity=stamp(&m);let mut held=None;let mut link=None;let mut digest=None;
            if m.is_dir(){ensure!(value.modules.iter().filter(|e|e.held.is_some()).count()<256,"provider observation descriptor bound");let f=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(&path)?;ensure!(stamp(&f.metadata()?)==identity,"provider directory open changed");held=Some(f);let mut children=Vec::new();for e in fs::read_dir(&path)?{children.push(e?.path());ensure!(children.len()+pending.len()+value.modules.len()<=4096,"provider observation entry bound");}children.sort();for p in children.into_iter().rev(){pending.push((p,depth+1));}}
            else if m.is_file(){ensure!(value.modules.iter().filter(|e|e.held.is_some()).count()<256,"provider observation descriptor bound");value.regular+=1;ensure!(m.len()<=16*1024*1024&&value.regular<=256,"provider observation file bound");let f=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&path)?;ensure!(stamp(&f.metadata()?)==identity,"provider file open changed");let key=(m.dev(),m.ino());let hash=if let Some(h)=unique.get(&key){h.clone()}else{total=total.checked_add(m.len()).context("provider bytes overflow")?;ensure!(total<=64*1024*1024,"provider observation byte bound");let h=bounded_hash(&f,m.len(),original,cancel)?;unique.insert(key,h.clone());h};digest=Some(hash);held=Some(f);}
            else if m.file_type().is_symlink(){let target=fs::read_link(&path)?;ensure!(target.as_os_str().len()<=4096,"provider link too long");link=Some(target);}
            else {anyhow::bail!("provider observation special file");}
            ensure!(stamp(&fs::symlink_metadata(&path)?)==identity,"provider entry changed");value.modules.push(ModuleEntry{path,held,identity,link,digest});
        }
        ensure!(value.regular>0,"physical provider payload required; empty image boundary is not a positive boot probe");value.modules.sort_by(|a,b|a.path.cmp(&b.path));let data=value.modules.iter().map(|e|(&e.path,&e.digest,&e.link)).collect::<Vec<_>>();value.payload_sha=sha(&canonical_json(&data)?);value.current()?;Ok(value)
    }
    fn ids(&self)->BTreeMap<String,NamespaceId>{self.ns.iter().map(|(n,_,id)|(n.clone(),id.clone())).collect()}
    fn current(&self)->Result<()> {ensure!(self.pid==std::process::id()&&self.kernel==kernel()?&&self.boot_id==read_boot(&self.boot)?&&self.boot_identity==nsid(&self.boot.metadata()?)&&self.boot_identity==nsid(&fs::metadata("/proc/sys/kernel/random/boot_id")?),"original kernel/boot/process changed");for(n,f,id)in &self.ns{ensure!(nsid(&f.metadata()?)==*id&&nsid(&fs::metadata(format!("/proc/self/ns/{n}"))?)==*id,"original host namespace changed");}
        for e in &self.modules{ensure!(stamp(&fs::symlink_metadata(&e.path)?)==e.identity,"provider payload metadata changed");if let Some(f)=&e.held{ensure!(stamp(&f.metadata()?)==e.identity,"held provider payload changed");}if let Some(link)=&e.link{ensure!(fs::read_link(&e.path)?==*link,"provider symlink changed");}}Ok(())}
    fn deep(&self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {self.current()?;let mut seen=BTreeSet::new();for e in &self.modules{if let (Some(f),Some(d))=(&e.held,&e.digest){if seen.insert((e.identity.dev,e.identity.ino)){ensure!(bounded_hash(f,e.identity.len,original,cancel)?==*d,"provider payload bytes changed");}}}self.current()}
}
struct Registered {registered:Arc<RegisteredEngineOwner>,config:Arc<HostIngressConfigOwner>}
impl Registered {fn current(&self)->Result<()>{self.registered.assert_current()?;self.config.assert_current()?;let b=&self.registered.data().binding;ensure!(b.config_sha256==self.config.config_sha256()&&b.home_id==self.config.data().home_id&&b.incarnation==self.config.data().incarnation&&b.runtime_manifest_sha256==self.config.data().runtime_manifest.manifest_sha256,"kernel probe original registration/config differs");Ok(())}}
struct ChildOwner {child:Child,pidfd:Option<OwnedFd>,exe:Option<File>,out:Vec<u8>,err:Vec<u8>,out_eof:bool,err_eof:bool,truncated:bool,status:Option<ExitStatus>}
impl ChildOwner {
    fn capture(child:Child)->Self{Self{child,pidfd:None,exe:None,out:Vec::new(),err:Vec::new(),out_eof:false,err_eof:false,truncated:false,status:None}}
    fn nonblocking(&self)->Result<()> {for fd in [self.child.stdout.as_ref().context("helper stdout absent")?.as_raw_fd(),self.child.stderr.as_ref().context("helper stderr absent")?.as_raw_fd()]{let flags=unsafe{libc::fcntl(fd,libc::F_GETFL)};ensure!(flags>=0&&unsafe{libc::fcntl(fd,libc::F_SETFL,flags|libc::O_NONBLOCK)}==0,"helper pipe nonblocking unavailable");}Ok(())}
    fn bind(&mut self,image:&Image)->Result<()>{self.nonblocking()?;let fd=unsafe{libc::syscall(libc::SYS_pidfd_open,self.child.id(),0)};ensure!(fd>=0,"actual helper pidfd unavailable");self.pidfd=Some(unsafe{OwnedFd::from_raw_fd(fd as i32)});image.current()?;self.exe=Some(image.file.try_clone()?);Ok(())}
    fn poll(&mut self)->Result<()>{self.nonblocking()?;
        fn read(r:&mut impl Read,bytes:&mut Vec<u8>,eof:&mut bool,truncated:&mut bool)->Result<()>{if *eof{return Ok(());}let mut chunk=[0u8;4096];for _ in 0..4{match r.read(&mut chunk){Ok(0)=>{*eof=true;return Ok(());},Ok(n)=>{let keep=n.min(LOG_LIMIT.saturating_sub(bytes.len()));bytes.extend_from_slice(&chunk[..keep]);if keep<n{*truncated=true;}},Err(e)if e.kind()==std::io::ErrorKind::WouldBlock=>return Ok(()),Err(e)if e.kind()==std::io::ErrorKind::Interrupted=>{},Err(e)=>return Err(e.into())}}Ok(())}
        read(self.child.stdout.as_mut().context("helper stdout absent")?,&mut self.out,&mut self.out_eof,&mut self.truncated)?;read(self.child.stderr.as_mut().context("helper stderr absent")?,&mut self.err,&mut self.err_eof,&mut self.truncated)?;
        if self.status.is_none(){self.status=self.child.try_wait()?;}if self.status.is_some()&&self.out_eof&&self.err_eof{drop(self.child.stdin.take());}Ok(())}
    fn exited(&self)->Result<bool>{let f=self.pidfd.as_ref().context("actual helper pidfd absent")?;let mut p=libc::pollfd{fd:f.as_raw_fd(),events:libc::POLLIN,revents:0};let n=unsafe{libc::poll(&mut p,1,0)};ensure!(n>=0,"helper pidfd poll failed");Ok(n==1&&p.revents&libc::POLLIN!=0)}
    fn settled(&self)->bool{self.status.is_some()&&self.out_eof&&self.err_eof}
    fn kill(&mut self){if self.status.is_none(){if let Some(f)=&self.pidfd{let _=unsafe{libc::syscall(libc::SYS_pidfd_send_signal,f.as_raw_fd(),libc::SIGKILL,std::ptr::null::<libc::siginfo_t>(),0)};}else{let _=self.child.kill();}}}
}
#[derive(Deserialize,Serialize)]#[serde(deny_unknown_fields)]
struct Nested {unshare_result:i32,unshare_errno:i32,unmount_errno:Option<i32>,payload_visible:bool}
#[derive(Deserialize,Serialize)]#[serde(deny_unknown_fields)]
struct Guest {format_version:u32,nonce:String,profile:String,uid:u32,gid:u32,all_capabilities_zero:bool,no_new_privileges:bool,mask_empty_readonly:bool,merged_alias_masked:bool,provider_tools_masked:bool,
    private_and_old_root_denied:bool,inherited_fd_closed:bool,namespaces:BTreeMap<String,NamespaceId>,interfaces:Vec<String>,seccomp_mode:u8,seccomp_filter_installed_by_fixture:bool,
    denial_errno:BTreeMap<String,i32>,nested_namespace:Nested,nested_child_reaped:bool,nested_pipe_eof:bool}
fn validate_guest(g:&Guest,nonce:&str,ids:&BTreeMap<String,NamespaceId>)->Result<()> {
    ensure!(g.format_version==1&&g.nonce==nonce&&g.profile==PROFILE&&g.uid==1500&&g.gid==1500&&g.all_capabilities_zero&&g.no_new_privileges&&g.mask_empty_readonly&&g.merged_alias_masked&&g.provider_tools_masked&&g.private_and_old_root_denied&&g.inherited_fd_closed&&g.nested_child_reaped&&g.nested_pipe_eof&&!g.nested_namespace.payload_visible,"guest kernel probe facts incomplete");
    ensure!(g.namespaces.keys().eq(ids.keys())&&["user","mnt","pid","net"].iter().all(|n|g.namespaces.get(*n)!=ids.get(*n))&&g.interfaces==vec!["lo".to_string()]&&matches!(g.seccomp_mode,0|2)&&!g.seccomp_filter_installed_by_fixture,"guest namespace/profile facts differ");
    let keys=BTreeSet::from(["mount","unmount_mask","setns","ptrace_init"]);ensure!(g.denial_errno.keys().map(String::as_str).collect::<BTreeSet<_>>()==keys&&g.denial_errno.values().all(|v|matches!(*v,libc::EPERM|libc::EACCES)),"actual guest denial observations incomplete");
    let nested=&g.nested_namespace;ensure!(if nested.unshare_result==0 {nested.unmount_errno.is_some_and(|v|matches!(v,libc::EPERM|libc::EACCES|libc::EINVAL))}else{nested.unshare_result==-1&&matches!(nested.unshare_errno,libc::EPERM|libc::EACCES|libc::EINVAL|libc::ENOSYS)},"nested namespace could uncover provider payload");Ok(())
}
struct State {
    original:FinalizationBudget,cancel:Arc<AtomicBool>,registered:Option<Registered>,base:PathBuf,expected_native:String,image:String,
    nonce:String,root:Option<PathBuf>,root_fd:Option<File>,anchors:retirement::Anchors,
    source_ref:Option<rudder_engine::protocol::ObjectRef>,profile_ref:Option<rudder_engine::protocol::ObjectRef>,source_stage:Option<rudder_engine::store::StagingLease>,observation:Option<rudder_engine::vfs::VfsObservation>,retirement:Option<retirement::Retirement>,retirement_cancel:Option<Arc<AtomicBool>>,native:Option<Image>,host:Option<Host>,store:Option<LocalStore>,manager:Option<VfsManager>,journal:Option<Journal>,issuance:Option<PreparedCapabilityIssuance>,
    broker_owner:BrokerThreadOwner,broker:Option<AgentBroker>,broker_joined:Option<BrokerThreadsSettled>,activation:Option<RetainedManagedExecutionActivation>,receipt:Option<HandoffReceipt>,control:Option<ManagedExecutionHandle>,handoff_joined:Option<WorkerJoined>,control_joined:Option<WorkerJoined>,backend:Option<Arc<BackendProcessWitness>>,shutdown:Option<ValidatedManagedShutdown>,stop:Option<StopAndCheckpoint>,child:Option<ChildOwner>,
    spec:Option<ExecutionSpec>,binding:Option<ManagedExecutionBinding>,guest:Option<Guest>,report:Option<Value>,stage:&'static str,attempted:bool,finished:bool,failed:bool,error:Option<anyhow::Error>,panic:Option<Box<dyn std::any::Any+Send>>,
}
impl State {
    fn current(&self)->Result<()> {time(&self.original,&self.cancel)?;if let Some(r)=&self.registered{r.current()?;}if let Some(n)=&self.native{n.current()?;}if let Some(h)=&self.host{h.current()?;}if let (Some(p),Some(f))=(&self.root,&self.root_fd){let m=f.metadata()?;let named=fs::symlink_metadata(p)?;ensure!(m.is_dir()&&named.is_dir()&&!named.file_type().is_symlink()&&(m.dev(),m.ino(),m.uid(),m.gid(),m.mode())==(named.dev(),named.ino(),1500,1500,0o40700),"original private kernel probe root changed");}Ok(())}
    fn prepare(&mut self)->Result<()> {
        self.stage="capture-native-and-host";self.current()?;private(&self.base)?;self.anchors.capture_base(&self.base)?;
        self.native=Some(Image::capture(&self.expected_native,&self.original,&self.cancel)?);self.host=Some(Host::capture(&self.original,&self.cancel)?);
        if let Some(r)=&self.registered {ensure!(self.host.as_ref().unwrap().boot_id==r.registered.boot_id(),"probe boot differs from actual registration");let own=File::open("/proc/self/exe")?;ensure!(stamp(&own.metadata()?)==self.native.as_ref().unwrap().identity,"registered probe native image is not the original engine");}
        let root=self.base.join(format!("kernel-probe-{}",self.nonce));self.root=Some(root.clone());mkdir(&root)?;self.root_fd=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&root)?);self.anchors.capture_root(self.root_fd.as_ref().unwrap(),root.file_name().unwrap())?;
        let authority=root.join("authority");mkdir(&authority)?;let home=root.join("home");mkdir(&home)?;self.stage="prepare-local-workspace";
        self.store=Some(LocalStore::open(root.join("cas"))?);self.anchors.capture_cas(self.store.as_ref().unwrap().root())?;
        self.manager=Some(VfsManager::open(root.join("vfs"),self.store.as_ref().unwrap().clone())?);self.anchors.capture_vfs(self.manager.as_ref().unwrap().root())?;
        let store=self.store.as_ref().unwrap();let stage=store.begin_staging(Duration::from_secs(120))?;self.source_stage=Some(stage.clone());let script=store.put_blob(GUEST.as_bytes())?;
        let directory=PagedDirectory::new(&LocalDirectoryStore::new(store,&stage.id)).build(0o755,vec![DirectoryEntry::new(b"kernel-probe.py",EntryNode::File{object:script.clone(),logical_size:script.size_bytes,mode:0o444})?])?;self.source_ref=Some(directory.clone());store.pin("kernel-probe-source",&directory)?;
        let profile=WorkspaceProfile::default();let profile_ref=store.put_blob(&canonical_json(&profile)?)?;self.profile_ref=Some(profile_ref.clone());store.pin("kernel-probe-workspace-profile",&profile_ref)?;
        self.journal=Some(Journal::open(authority.join("journal.sqlite"))?);let journal=self.journal.as_mut().unwrap();let fence=journal.create_project("kernel-probe","kernel-probe","fixture-only",now_ms()?)?;let owner=OwnerEpoch{owner:fence.owner_id.clone(),epoch:fence.epoch};
        let view_id=Uuid::new_v4().to_string();let execution_id=Uuid::new_v4().to_string();let operation_id=Uuid::new_v4().to_string();let request_id=Uuid::new_v4().to_string();let manager=self.manager.as_ref().unwrap();manager.prepare_paged(&directory)?;let view=manager.fork_paged(&view_id,&directory,owner.clone())?;self.anchors.capture_view(&view_id)?;
        self.observation=Some(view.observe_source(&profile.source_exclusions(),&self.nonce,&ObservationBudget::default())?);let binding=ObservationBinding{format_version:1,attempt:AttemptContext{execution_id:execution_id.clone(),operation_id:operation_id.clone()},view_id:view_id.clone(),owner:owner.clone(),namespace:view.dirty_cursor()?.namespace,profile:profile_ref,exclusion_policy_digest:exclusion_policy_digest(&profile.source_exclusions())?};
        // Genuine local VFS ownership transfers to the fresh helper. Keep the
        // actual manager/CAS and exact sealed namespace; do not keep its lock.
        drop(view);let run="kernel-probe-run";let epoch=journal.observe_run_attempt(&fence,run,&execution_id,now_ms()?)?;
        let grant=CapabilityGrant{principal:"kernel-probe-agent".into(),run_id:run.into(),run_epoch:epoch,operations:BTreeSet::from(["workspace:observe".into()]),recipients:BTreeMap::new(),scopes:BTreeSet::from([execution::workspace_scope(&binding)?]),topics:BTreeSet::new(),expires_at_ms:None};
        self.issuance=Some(journal.prepare_capability_issuance(&fence,&grant,now_ms()?)?);journal.issue_prepared_capability(self.issuance.as_mut().unwrap(),now_ms()?)?;
        let identity=AgentIdentityFile{format_version:1,project_id:fence.project_id.clone(),run_id:run.into(),run_epoch:epoch,owner_epoch:fence.epoch,token:self.issuance.as_ref().unwrap().acknowledged_token()?.to_string()};
        let socket=crate::agent_broker::socket_path(&home,&fence.project_id,run,&execution_id);self.anchors.before_projection(&socket)?;let identity_file=crate::agent_broker::project_identity(&socket,&identity)?;self.anchors.projection_ack(&socket)?;self.current()?;self.stage="spawn-real-workspace-broker";
        self.broker=Some(AgentBroker::spawn_retaining(BrokerConfig{journal_path:authority.join("journal.sqlite"),identity_file,socket_path:socket.clone()},&mut self.broker_owner)?);self.anchors.broker_ack()?;
        let network=NetworkProfileV1{format_version:1,execution_profile:NetworkExecutionProfile::CloudUid1500LinuxV1,binding:NetworkBinding{project_id:fence.project_id.clone(),owner:owner.owner.clone(),owner_epoch:NetworkEpoch::new(owner.epoch)?,run_id:run.into(),run_epoch:NetworkEpoch::new(epoch)?,request_id:request_id.clone(),execution_id:execution_id.clone(),operation_id:operation_id.clone(),view_id:view_id.clone(),network_id:Uuid::new_v4().to_string()},mode:NetworkMode::OfflineV1,policy:None,policy_sha256:None,readiness_nonce:Uuid::new_v4().to_string(),budgets:NetworkBudgets{max_connections:0,max_dns_jobs:0,max_request_bytes:0,max_buffer_bytes:0,max_bytes_per_connection:0,max_total_bytes:0,connect_timeout_ms:0,idle_timeout_ms:0,total_timeout_ms:0}};
        let input=json!({"profile":PROFILE,"nonce":self.nonce,"host_namespaces":self.host.as_ref().unwrap().ids(),"private_root":root,"forbidden_paths":[authority.join("private-sentinel"),root.join("cas"),root.join("vfs")]});
        let mut runtime=vec![ReadOnlyRuntime{source:"/usr".into(),guest_path:"/usr".into()}];if Path::new("/etc/ld.so.cache").is_file(){runtime.push(ReadOnlyRuntime{source:"/etc/ld.so.cache".into(),guest_path:"/etc/ld.so.cache".into()});}
        let spec=ExecutionSpec{format_version:4,execution_id,operation_id,execution_root:root.join("executions"),manager_root:manager.root().into(),store_root:store.root().into(),authority_roots:vec![authority.clone()],view_id,owner,command:vec!["/usr/bin/python3".into(),"-I".into(),"-B".into(),"/workspace/kernel-probe.py".into(),String::from_utf8(canonical_json(&input)?)?],environment:BTreeMap::new(),runtime,home_files:Vec::new(),broker_directory:socket.parent().map(Path::to_path_buf),broker_binding:Some(BrokerBinding{project_id:fence.project_id,run_id:run.into(),run_epoch:epoch,owner_epoch:fence.epoch}),workspace_observation:Some(WorkspaceObservationProfile{binding,endpoint_directory:root.join("workspace-rpc"),application:None}),profile:Some(ExecutionProfileV4{format_version:1,request_id,network,workspace:ExecutionWorkspaceSelectionV4::LocalComplete{},startup:None}),timeout_seconds:20};
        spec.validate()?;negative_inputs(&spec)?;
        let broker=spec.broker_binding.as_ref().unwrap();let binding=ManagedExecutionBinding{format_version:3,request_id:spec.profile.as_ref().unwrap().request_id.clone(),run_id:broker.run_id.clone(),run_epoch:broker.run_epoch,execution_id:spec.execution_id.clone(),operation_id:spec.operation_id.clone(),view_id:spec.view_id.clone(),owner:spec.owner.clone(),execution_profile:"offline-command-v1".into(),frontend_start:ManagedFrontendStart::Immediate,server_command:None,guest_socket:"/tmp/cx.sock".into(),profile:spec.profile.clone()};binding.validate(&spec)?;
        self.binding=Some(binding);self.spec=Some(spec);self.current()
    }
    fn spawn(&mut self)->Result<()> {self.stage="spawn-actual-native-helper";self.current()?;let spec=self.spec.as_ref().unwrap();self.activation=Some(RetainedManagedExecutionActivation::new(ManagedExecutionHandoff::new(spec,self.binding.as_ref().unwrap().clone())?));
        let sentinel=self.root.as_ref().unwrap().join("authority/private-sentinel");let mut file=OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&sentinel)?;file.write_all(b"kernel probe private descriptor sentinel")?;file.sync_all()?;
        let raw=unsafe{libc::fcntl(file.as_raw_fd(),libc::F_DUPFD_CLOEXEC,256)};ensure!(raw>=256,"private sentinel FD unavailable");let inherited=unsafe{OwnedFd::from_raw_fd(raw)};
        // Exec the SAME verified open ELF. /proc/PID/exe readback would race
        // the real helper's nondumpable floor and is not an authority fallback.
        let mut command=Command::new(format!("/proc/self/fd/{}",self.native.as_ref().unwrap().file.as_raw_fd()));command.args(self.activation.as_ref().unwrap().helper_args()).env_clear().env("HOST_ENV_MUST_NOT_LEAK","fixture-data").current_dir("/").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());let raw=inherited.as_raw_fd();
        unsafe{command.pre_exec(move||{if libc::dup3(raw,200,0)!=200{return Err(std::io::Error::last_os_error());}Ok(())});}
        self.child=Some(ChildOwner::capture(command.spawn()?));drop(inherited);self.child.as_mut().unwrap().bind(self.native.as_ref().unwrap())?;
        self.activation.as_mut().unwrap().activate_bounded(self.child.as_ref().unwrap().child.id(),&self.original,self.cancel.clone())?;
        let(receipt,control)=self.activation.as_mut().unwrap().take_live()?;self.receipt=Some(receipt);self.control=Some(control);self.current()
    }
    fn run_inner(&mut self)->Result<()> {self.prepare()?;self.spawn()?;self.stage="actual-guest-proof-and-managed-stop";let mut transfer=false;
        loop{self.current()?;
            while let Some(event)=self.control.as_ref().unwrap().try_event(){match event{
                ManagedExecutionEvent::BackendReady{process}=>{let b=self.binding.as_ref().unwrap();ensure!(self.backend.is_none()&&process.matches_attempt(&b.run_id,&b.execution_id,"offline-command-v1")&&!process.authenticated_start_required(),"actual backend witness differs");process.check_current()?;self.backend=Some(process);},
                ManagedExecutionEvent::Shutdown(receipt)=>{ensure!(self.shutdown.is_none()&&Some(receipt.request())==self.stop.as_ref(),"shutdown not exact original stop");self.shutdown=Some(receipt);},
                ManagedExecutionEvent::SourceShutdown(_)|ManagedExecutionEvent::Uncertain=>anyhow::bail!("actual managed kernel probe shutdown uncertain"),
            }}
            let child=self.child.as_mut().unwrap();child.poll()?;ensure!(!child.truncated,"kernel probe pipe bound exceeded");
            if !transfer{if let Some(result)=self.receipt.as_ref().unwrap().try_result(){result.map_err(anyhow::Error::msg)?;transfer=true;}}
            if self.guest.is_none(){if let Some(raw)=one_guest_frame(&child.out)?{ensure!(transfer,"guest proof before actual handoff acknowledgment");let guest:Guest=serde_json::from_slice(raw)?;validate_guest(&guest,&self.nonce,&self.host.as_ref().unwrap().ids())?;self.guest=Some(guest);}}
            if self.stop.is_none()&&self.guest.is_some()&&self.backend.is_some(){self.backend.as_ref().unwrap().check_current()?;let b=self.binding.as_ref().unwrap();let stop=StopAndCheckpoint{request_id:Uuid::new_v4().to_string(),execution_id:b.execution_id.clone(),operation_id:b.operation_id.clone(),view_id:b.view_id.clone(),run_id:b.run_id.clone(),run_epoch:b.run_epoch,owner:b.owner.clone()};
                self.stop=Some(stop);self.control.as_ref().unwrap().stop_and_checkpoint(self.stop.as_ref().unwrap().clone())?;
            }
            if child.settled()&&self.shutdown.is_some(){ensure!(self.guest.is_some()&&transfer&&child.exited()?,"actual helper reap or EOF absent");break;}
            std::thread::sleep(self.original.phase_limit(Duration::from_millis(2))?);
        }
        self.stage="join-actual-workers-and-retire-fixture-grant";self.settle(&self.original.clone())?;self.host.as_ref().unwrap().deep(&self.original,&self.cancel)?;self.current()?;
        let spec=self.spec.as_ref().unwrap();let recovered=execution::read_execution_recovery(&spec.execution_root,&spec.execution_id)?.context("actual execution record absent")?;let record=recovered.record();
        ensure!(record.execution_id()==spec.execution_id&&record.operation_id()==spec.operation_id&&record.view_id()==spec.view_id&&record.owner()==&spec.owner&&record.request_digest()==sha(&canonical_json(spec)?)&&record.supervisor_pid()==self.child.as_ref().unwrap().child.id()&&record.phase()==execution::ExecutionPhase::Checkpointed&&record.agent_exit()==Some(143)&&record.checkpoint().is_some(),"record differs from exact original actual execution");
        ensure!(self.child.as_ref().unwrap().status.as_ref().unwrap().code()==record.agent_exit()&&canonical_json(self.shutdown.as_ref().unwrap().record())?==canonical_json(record)?,"helper exit/typed shutdown/record differ");
        ensure!(one_guest_frame(&self.child.as_ref().unwrap().out)?.is_some(),"final guest frame absent");
        self.report=Some(json!({"format_version":1,"scope":"actual-provider-kernel-guest-component-only","qualified":true,"profile":PROFILE,"kernel":self.host.as_ref().unwrap().kernel,"boot_id":self.host.as_ref().unwrap().boot_id,"native_sha256":self.native.as_ref().unwrap().sha,"image":self.image,"guest":self.guest,"provider_regular_files":self.host.as_ref().unwrap().regular,"host_provider_payload_sha256":self.host.as_ref().unwrap().payload_sha,"host_provider_payload_unchanged":true,"guest_source_sha256":sha(GUEST.as_bytes()),"actual_exec_via_verified_elf_fd":true,"actual_backend_witness":true,"actual_managed_shutdown":true,"actual_helper_reaped":true,"helper_exit":self.child.as_ref().unwrap().status.as_ref().unwrap().code(),"stdout_eof":true,"stderr_eof":true,"handoff_worker_tls_joined":true,"control_worker_tls_joined":true,"broker_workers_tls_joined":true,"runtime_negative_inputs_refused":true,"record_sha256":sha(&canonical_json(record)?),"production_startup":false,"source_or_project_authority":false,"spec3":false,"models":false}));self.finished=true;self.stage="completed-original-probe";Ok(())
    }
    fn settle(&mut self,budget:&FinalizationBudget)->Result<()> {
        budget.remaining()?;if let Some(child)=&mut self.child{child.poll()?;ensure!(child.settled()&&(child.pidfd.is_none()||child.exited()?),"actual helper/stdio still owned");}
        if let Some(control)=&mut self.control{if self.control_joined.is_none(){self.control_joined=Some(control.await_closed(budget.remaining()?)?);}}
        if let Some(receipt)=&mut self.receipt{if self.handoff_joined.is_none(){self.handoff_joined=Some(receipt.await_closed(budget.remaining()?)?);}}
        if let(Some(a),Some(b))=(&self.control_joined,&self.handoff_joined){ensure!(a.same_worker(b),"actual handoff/control TLS joins differ");}
        if self.control.is_none(){if let Some(activation)=&mut self.activation{activation.settle_undispatched_off_app(budget)?;}}
        self.broker_owner.close();if self.broker_joined.is_none(){self.broker_joined=Some(self.broker_owner.await_closed(budget.remaining()?)?);}
        if let (Some(journal),Some(issuance))=(&mut self.journal,&mut self.issuance){if !issuance.is_retired(){journal.retire_prepared_capability(issuance,now_ms()?)?;}}
        budget.remaining()?;Ok(())
    }
    fn settled(&self)->bool{self.child.as_ref().is_none_or(ChildOwner::settled)&&(self.receipt.is_none()||self.handoff_joined.is_some())&&(self.control.is_none()||self.control_joined.is_some())&&(self.control.is_some()||self.activation.as_ref().is_none_or(RetainedManagedExecutionActivation::is_settled))&&self.broker_joined.is_some()&&self.issuance.as_ref().is_none_or(PreparedCapabilityIssuance::is_retired)}
    fn stop(&mut self){if let Some(control)=&self.control{control.close();}if let Some(activation)=&self.activation{activation.close();}if let Some(receipt)=&self.receipt{receipt.close();}if let Some(child)=&mut self.child{child.kill();}self.broker_owner.close();}
    fn recovery_data(&self)->Value {json!({"scope":"kernel-probe-recovery-data-only","stage":self.stage,"finished":self.finished,"failed":self.failed,"settled":self.settled(),"child_status":self.child.as_ref().and_then(|c|c.status.as_ref()).and_then(ExitStatus::code),"output_truncated":self.child.as_ref().map(|c|c.truncated),"stdout_eof":self.child.as_ref().map(|c|c.out_eof),"stderr_eof":self.child.as_ref().map(|c|c.err_eof),"stdout_sha256":self.child.as_ref().map(|c|sha(&c.out)),"stderr_sha256":self.child.as_ref().map(|c|sha(&c.err)),"startup_capability":false,"replay_authorized":false,"own_fixture_retirement":self.retirement.as_ref().map(retirement::Retirement::recovery_data),"own_fixture_retirement_ack":self.retirement.as_ref().is_some_and(retirement::Retirement::acknowledged),"own_fixture_retired":self.retirement.as_ref().is_some_and(retirement::Retirement::done)})}
}
fn negative_inputs(spec:&ExecutionSpec)->Result<()> {for (source,guest)in [("/usr/lib/modules","/opt/runtime/modules"),("/usr","/opt/runtime/system"),("/usr/lib","/usr/local/lib"),("/usr/share","/usr/lib"),("/usr/share","/usr/lib/modules")]{let mut s=spec.clone();s.runtime=vec![ReadOnlyRuntime{source:source.into(),guest_path:guest.into()}];ensure!(s.validate().is_err(),"runtime provider boundary negative accepted");}Ok(())}
fn one_guest_frame(bytes:&[u8])->Result<Option<&[u8]>> {let prefix=b"RUDDER_PROVIDER_KERNEL_GUEST ";let mut frames=bytes.split(|b|*b==b'\n').filter(|line|line.starts_with(prefix));let Some(frame)=frames.next()else{return Ok(None)};ensure!(frames.next().is_none(),"duplicate guest frame");if !bytes.ends_with(b"\n"){return Ok(None);}ensure!(frame.len()<=16*1024,"guest frame bound");Ok(Some(&frame[prefix.len()..]))}
/// Staged before queue/effects; borrowed run/recovery never loses actual owners.
/// This is a one-attempt, one-per-process startup component, not a scheduler.
#[must_use="retain original kernel probe through explicit settlement"]
pub(crate) struct KernelStartupProbe {state:Option<Box<State>>}
/// Genuine completed current-boot probe + ORIGINAL registered context. No
/// Deserialize/Clone/public DATA constructor. It grants no source/project/run.
pub(crate) struct KernelStartupOwner {state:Mutex<Option<Box<State>>>,registration:Arc<RegisteredEngineOwner>,fenced:AtomicBool,startup_closed:AtomicBool,retired:AtomicBool}
impl KernelStartupProbe {
    pub(crate) fn begin_registered_retaining(out:&mut Option<Self>,registered:Arc<RegisteredEngineOwner>,config:Arc<HostIngressConfigOwner>,original:FinalizationBudget,cancel:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"kernel probe output occupied");
        let expected=registered.data().binding.native_sha256.clone();let image=registered.data().binding.image.clone();let base=config.data().state_directory.clone();
        Self::stage(out,Some(Registered{registered,config}),base,expected,image,original,cancel)
    }
    fn stage(out:&mut Option<Self>,registered:Option<Registered>,base:PathBuf,expected_native:String,image:String,original:FinalizationBudget,cancel:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"kernel probe output occupied");ensure!(!ONE_PROBE.swap(true,Ordering::AcqRel),"original kernel probe already reserved; no automatic retry");
        *out=Some(Self{state:Some(Box::new(State{original,cancel,registered,base,expected_native,image,nonce:Uuid::new_v4().to_string(),root:None,root_fd:None,anchors:retirement::Anchors::default(),source_ref:None,profile_ref:None,source_stage:None,observation:None,retirement:None,retirement_cancel:None,native:None,host:None,store:None,manager:None,journal:None,issuance:None,broker_owner:BrokerThreadOwner::new(),broker:None,broker_joined:None,activation:None,receipt:None,control:None,handoff_joined:None,control_joined:None,backend:None,shutdown:None,stop:None,child:None,spec:None,binding:None,guest:None,report:None,stage:"staged-before-effects",attempted:false,finished:false,failed:false,error:None,panic:None}))});
        let s=out.as_mut().unwrap().state.as_mut().unwrap();let checked=parent_floor().and_then(|_|s.current());if let Err(error)=checked{s.failed=true;s.error=Some(error);return Err(fault());}Ok(())
    }
    pub(crate) fn run_retaining(&mut self)->Result<()> {
        let s=self.state.as_mut().context("original kernel probe transferred")?;ensure!(!s.attempted&&!s.failed,"original kernel probe already attempted or fenced");s.attempted=true;
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||s.run_inner()));match result{Ok(Ok(()))=>Ok(()),Ok(Err(error))=>{s.error=Some(error);s.failed=true;s.stop();Err(fault())},Err(payload)=>{s.panic=Some(payload);s.failed=true;s.stop();Err(fault())}}
    }
    /// A distinct caller-supplied cleanup budget never renews a failed probe or
    /// permits another child/effect attempt. Keep this carrier on every error.
    pub(crate) fn recover_retaining(&mut self,cleanup:&FinalizationBudget)->Result<()> {
        let s=self.state.as_mut().context("original kernel probe transferred")?;ensure!(s.retirement.is_none(),"own-fixture retirement has retained progress; no recovery replay");s.failed=true;s.stop();loop{cleanup.remaining()?;if let Some(child)=&mut s.child{child.poll()?;if !child.settled(){std::thread::sleep(cleanup.phase_limit(Duration::from_millis(2))?);continue;}}s.settle(cleanup)?;return Ok(());}
    }
    /// OFF App. Only a successfully settled original probe can retire normally.
    /// Retain this carrier on Err, including any known late cleanup ACK.
    pub(crate) fn retire_own_fixture_retaining(&mut self,original:&FinalizationBudget,cancel:&Arc<AtomicBool>)->Result<KernelProbeFixtureRetired> {
        retirement::retire(self.state.as_mut().context("original probe transferred")?,original,cancel)
    }
    pub(crate) fn recovery_data(&self)->Value {self.state.as_ref().map_or(json!({"scope":"kernel-probe-transferred-data-only"}),|s|s.recovery_data())}
    /// Stage the actual completed carrier BEFORE fallible post-ACK checks. A
    /// component-only test run cannot issue this owner or attach registration.
    pub(crate) fn take_startup_owner_retaining(&mut self,out:&mut Option<Arc<KernelStartupOwner>>)->Result<()> {
        ensure!(out.is_none(),"kernel startup owner output occupied");let s=self.state.as_ref().context("original probe transferred")?;
        ensure!(s.attempted&&s.finished&&!s.failed&&s.error.is_none()&&s.panic.is_none()&&s.settled()&&s.registered.is_some(),"actual registered probe success/joins absent");s.current()?;
        let registration=s.registered.as_ref().unwrap().registered.clone();let original=s.original.clone();
        *out=Some(Arc::new(KernelStartupOwner{state:Mutex::new(self.state.take()),registration,fenced:AtomicBool::new(false),startup_closed:AtomicBool::new(false),retired:AtomicBool::new(false)}));
        let owner=out.as_ref().unwrap();let adopted=owner.assert_current_off_app().and_then(|_|original.remaining().map(|_|()));if adopted.is_err(){owner.fenced.store(true,Ordering::Release);}adopted
    }
    #[cfg(test)]
    fn begin_component_retaining(out:&mut Option<Self>,base:PathBuf,expected_native:String,image:String,original:FinalizationBudget)->Result<()> {
        ensure!(image.starts_with("registry.fly.io/")&&image.split_once("@sha256:").is_some_and(|(_,h)|h.len()==64&&h.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))),"component exact image DATA invalid");Self::stage(out,None,base,expected_native,image,original,Arc::new(AtomicBool::new(false)))
    }
    #[cfg(test)]
    fn component_report(&self)->Result<Value>{let s=self.state.as_ref().context("probe transferred")?;ensure!(s.finished&&!s.failed&&s.settled()&&s.registered.is_none(),"actual component success absent");Ok(s.report.as_ref().context("component report absent")?.clone())}
    #[cfg(test)]
    fn save_private_diagnostics(&self)->Result<()> {let s=self.state.as_ref().context("probe transferred")?;if let(Some(root),Some(child))=(&s.root,&s.child){for(name,bytes)in[("helper.stdout.private",&child.out),("helper.stderr.private",&child.err)]{let mut f=OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(root.join(name))?;f.write_all(bytes)?;f.sync_all()?;}}Ok(())}
}
impl KernelStartupOwner {
    /// Pure App check: no lock, filesystem, clock sample, wait or channel IO.
    pub(crate) fn check_startup_admission_on_app(&self)->Result<()> {
        ensure!(!self.startup_closed.load(Ordering::Acquire)&&!self.retired.load(Ordering::Acquire)&&!self.fenced.load(Ordering::Acquire),"new kernel startup admission closed");Ok(())
    }
    /// Only NEW startup admission closes. Existing source/task/cleanup owners
    /// remain independent and no live user agent is stopped by this method.
    pub(crate) fn close_startup_admission(&self) {self.startup_closed.store(true,Ordering::Release);}
    pub(crate) fn assert_startup_admission_off_app(&self)->Result<()> {self.check_startup_admission_on_app()?;self.assert_current_off_app()?;self.check_startup_admission_on_app()}
    pub(crate) fn retire_own_fixture_retaining(&self,original:&FinalizationBudget,cancel:&Arc<AtomicBool>)->Result<KernelProbeFixtureRetired> {
        ensure!(self.startup_closed.load(Ordering::Acquire),"close NEW startup admission before own-fixture retirement");
        let mut state=loop {
            time(original,cancel)?;
            match self.state.try_lock() {
                Ok(held)=>break held,
                Err(std::sync::TryLockError::WouldBlock)=>std::thread::sleep(original.phase_limit(Duration::from_millis(2))?),
                Err(std::sync::TryLockError::Poisoned(_))=>return Err(fault()),
            }
        };
        let held=state.as_mut().context("original kernel fixture absent")?;
        let result=retirement::retire(held,original,cancel);
        // A completed cleanup ACK stays held even when the final budget check
        // fails. Neither that ACK nor a retry reopens startup admission.
        if held.retirement.as_ref().is_some_and(retirement::Retirement::acknowledged){self.retired.store(true,Ordering::Release);}
        result
    }
    /// OFF App physical custody, independent of the NEW-startup closed gate.
    /// The expired original operation deadline is not renewed or reapplied.
    pub(crate) fn assert_current_off_app(&self)->Result<()> {let result=(||->Result<()>{ensure!(!self.fenced.load(Ordering::Acquire)&&!self.retired.load(Ordering::Acquire),"kernel startup owner fenced/retired");let held=self.state.lock().map_err(|_|fault())?;let s=held.as_ref().context("kernel startup owner disposed")?;ensure!(s.finished&&!s.failed&&s.settled()&&s.error.is_none()&&s.panic.is_none(),"actual successful probe custody absent");
        let r=s.registered.as_ref().context("component probe grants no startup owner")?;r.current()?;s.native.as_ref().context("actual native image absent")?.current()?;let host=s.host.as_ref().context("actual boot owner absent")?;host.current()?;ensure!(host.boot_id==r.registered.boot_id()&&s.image==r.registered.data().binding.image&&s.expected_native==r.registered.data().binding.native_sha256,"original registered kernel/image binding changed");Ok(())})();if result.is_err(){self.fenced.store(true,Ordering::Release);}result}
    pub(crate) fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.registration}
    pub(crate) fn profile(&self)->&'static str{PROFILE}
    pub(crate) fn observation_data(&self)->Result<Value>{self.assert_current_off_app()?;let held=self.state.lock().map_err(|_|fault())?;Ok(held.as_ref().unwrap().report.as_ref().unwrap().clone())}
}
fn retain_until_process_teardown(state:Option<Box<State>>){if let Some(state)=state{if state.retirement.as_ref().is_some_and(retirement::Retirement::done){return;}let raw=Box::into_raw(state);let _=RECOVERY.compare_exchange(std::ptr::null_mut(),raw,Ordering::AcqRel,Ordering::Acquire);/* One reserved probe; even impossible overlap never drops unknown custody. */}}
impl Drop for KernelStartupProbe {fn drop(&mut self){retain_until_process_teardown(self.state.take());}}
impl Drop for KernelStartupOwner {fn drop(&mut self){retain_until_process_teardown(self.state.get_mut().unwrap_or_else(|p|p.into_inner()).take());}}

#[cfg(test)]
#[path="provider_kernel_qualification_tests.rs"]
mod tests;
