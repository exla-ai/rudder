//! Child of engine_host. Source-only genesis of a NEW logical PG namespace.
//! This owner is deliberately NOT EngineOwnership/EngineSourceLease and exposes
//! no scheduler, run, helper, App or task admission API.
use super::*;
use anyhow::{ensure,Context,Result};
use crate::{managed_machine_cas::ManagedMachineCas,managed_fresh_environment::ManagedFreshEnvironment};
use rudder_native::engine_registration::FreshProjectPreparation;
use rudder_engine::{execution::finalization_budget::FinalizationBudget,journal::ManagedProjectNamespace};
use jj_lib::{git_backend::GitBackend,object_id::ObjectId as _,repo::{Repo,ReadonlyRepo},workspace::Workspace,ref_name::WorkspaceName};
use std::{ffi::CString,os::fd::{AsRawFd,FromRawFd}};

struct Directory {path:PathBuf,file:File,id:(u64,u64)}
impl Directory {
    fn open_existing_root(path:PathBuf)->Result<Arc<Self>> {let file=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&path)?;let m=file.metadata()?;
        let value=Arc::new(Self{path,file,id:(m.dev(),m.ino())});value.current()?;Ok(value)}
    fn current(&self)->Result<()> {for m in [self.file.metadata()?,fs::symlink_metadata(&self.path)?]{ensure!(m.is_dir()&&!m.file_type().is_symlink()&&(m.dev(),m.ino())==self.id&&m.uid()==unsafe{libc::geteuid()}&&m.gid()==unsafe{libc::getegid()}&&m.mode()&0o7777==0o700,"original fresh directory custody changed");}ensure!(self.path.canonicalize()?==self.path,"fresh directory redirects");Ok(())}
}
/// Parent FD and intent are retained before mkdirat. No existing name is ever
/// adopted, even after a returned error/unknown outcome.
struct NewDirectory {parent:Arc<Directory>,name:String,attempted:bool,ack:bool,directory:Option<Arc<Directory>>}
impl NewDirectory {
    fn stage(parent:Arc<Directory>,name:&str)->Result<Self> {ensure!(!name.is_empty()&&!matches!(name,"."|"..")&&name.bytes().all(|b|b.is_ascii_alphanumeric()||b"-_.".contains(&b)),"unsafe fixed fresh component");parent.current()?;Ok(Self{parent,name:name.into(),attempted:false,ack:false,directory:None})}
    fn create_retaining(&mut self,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;self.parent.current()?;ensure!(!self.attempted,"original fresh mkdir already attempted");let name=CString::new(self.name.as_str())?;self.attempted=true;
        ensure!(unsafe{libc::mkdirat(self.parent.file.as_raw_fd(),name.as_ptr(),0o700)}==0,"fresh directory exists or original creation unknown; retain recovery");self.ack=true;
        let fd=unsafe{libc::openat(self.parent.file.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW)};ensure!(fd>=0,"original fresh directory open failed");
        let file=unsafe{File::from_raw_fd(fd)};let m=file.metadata()?;self.directory=Some(Arc::new(Directory{path:self.parent.path.join(&self.name),file,id:(m.dev(),m.ino())}));
        let directory=self.directory.as_ref().unwrap();directory.current()?;ensure!(fs::read_dir(&directory.path)?.next().is_none(),"original new fresh directory is not empty");directory.file.sync_all()?;self.parent.file.sync_all()?;self.parent.current()?;time(original,cancel)
    }
    fn ready(&self)->Result<Arc<Directory>> {ensure!(self.ack,"original fresh directory ACK absent");let dir=self.directory.as_ref().context("original fresh directory carrier absent")?;dir.current()?;Ok(dir.clone())}
}
/// Sealed original namespace custody for ManagedStore's fresh-only overload.
/// It is not created from a path/receipt, not Serde/Clone, and not ACTIVE.
/// Actual lock and held SQLite keeper outlive all store/template/cache aliases.
pub(crate) struct FreshNamespaceCustody {preparation:Arc<FreshProjectPreparation>,machine:Arc<ManagedMachineCas>,directories:Vec<Arc<Directory>>,
    database:Arc<HeldJournalFile>,database_id:(u64,u64),lock:Arc<CheckoutLock>,namespace:ManagedProjectNamespace,floor_attempted:AtomicBool,floor_ack:std::sync::OnceLock<crate::managed_store::OriginalFreshFloorAck>}
impl FreshNamespaceCustody {
    pub(crate) fn namespace(&self)->&ManagedProjectNamespace{&self.namespace}
    pub(crate) fn claim_floor_once(&self)->Result<()>{ensure!(!self.floor_attempted.swap(true,Ordering::AcqRel),"fresh floor original effect already attempted");Ok(())}
    pub(crate) fn acknowledge_floor(&self,proof:crate::managed_store::OriginalFreshFloorAck)->Result<()> {ensure!(proof.matches(self.preparation.prepare()),"fresh original floor ACK belongs to another request");self.floor_ack.set(proof).map_err(|_|anyhow::anyhow!("fresh original floor ACK already retained"))}
    pub(crate) fn floor_acknowledged(&self)->bool{self.floor_ack.get().is_some_and(|proof|proof.matches(self.preparation.prepare()))}
    pub(crate) fn check_custody(&self)->Result<()> {
        self.machine.assert_custody()?;self.preparation.assert_current()?;ensure!(self.namespace==self.preparation.prepare().namespace,"fresh namespace differs from original PG reservation");
        for d in &self.directories{d.current()?;}let m=self.lock.metadata()?;let n=fs::symlink_metadata(self.namespace.repository.join(".rudder/engine/owner.lock"))?;
        ensure!(m.is_file()&&n.is_file()&&!n.file_type().is_symlink()&&m.uid()==1500&&n.uid()==1500&&m.nlink()==1&&n.nlink()==1&&(m.dev(),m.ino())==(n.dev(),n.ino()),"original fresh checkout lock changed");
        self.database.check_current()?;Ok(())
    }
    pub(crate) fn check_journal(&self,journal:&Journal)->Result<()> {
        self.check_custody()?;ensure!(journal.database_path()?==self.namespace.journal,"fresh Journal differs from held namespace");let supplied=journal.hold_database_file()?;let m=fs::symlink_metadata(journal.database_path()?)?;
        ensure!((m.dev(),m.ino())==self.database_id,"fresh Journal inode differs from actual held original");supplied.check_current()?;self.database.check_current()?;Ok(())
    }
}
/// Mutable original carrier lives outside catch/unwind/failure delivery. A
/// failed source-only initializer keeps partial dirs/Journal/jj/history/lock.
pub(crate) struct FreshLocalNamespace {preparation:Arc<FreshProjectPreparation>,environment:Arc<ManagedFreshEnvironment>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    parent:Option<Arc<Directory>>,directories:Vec<NewDirectory>,marker:Option<File>,marker_attempted:bool,marker_ack:bool,journal:Option<Journal>,held:Option<Arc<HeldJournalFile>>,lock_file:Option<File>,lock:Option<Arc<CheckoutLock>>,
    fence:Option<OwnerFence>,prepare_receipt:Option<rudder_engine::journal::ManagedProjectReceipt>,store:Option<crate::managed_store::ManagedStore>,custody:Option<Arc<FreshNamespaceCustody>>,workspace:Option<Workspace>,repository:Option<Arc<ReadonlyRepo>>,
    head:Option<rudder_vcs_bridge::GitCommitId>,active_fence:Option<OwnerFence>,active_record:Option<OwnerRecord>,transfer_attempted:bool,promoted:bool,attempted:bool,journal_attempted:bool,project_attempted:bool,prepare_attempted:bool,acknowledged:bool}
impl FreshLocalNamespace {
    pub(crate) fn capture_retaining(out:&mut Option<Self>,preparation:Arc<FreshProjectPreparation>,environment:Arc<ManagedFreshEnvironment>,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        ensure!(out.is_none(),"fresh namespace original carrier occupied");time(&original,&cancelled)?;preparation.assert_current()?;environment.current(&original,&cancelled)?;
        ensure!(Arc::ptr_eq(&preparation,environment.preparation()),"fresh source differs from original environment preparation");let ns=&preparation.prepare().namespace;
        let project=environment.machine().config().data().projects.iter().find(|p|p.alias==preparation.data().workspace_id).context("fresh namespace outside actual Root config")?;
        let base=Path::new("/workspace/fresh-v4").join(&ns.project_id);ensure!(project.prepare==*preparation.prepare()&&project.checkout==ns.repository&&project.home==ns.home&&ns.home==base.join("home")&&ns.repository==base.join("repository")&&project.endpoints==base.join("endpoints")&&project.cas==environment.machine().config().data().state_directory.join("cas"),"fresh namespace changed committed/configured logical UUID binding");
        *out=Some(Self{preparation,environment,original,cancelled,parent:None,directories:Vec::with_capacity(8),marker:None,marker_attempted:false,marker_ack:false,lock_file:None,lock:None,journal:None,held:None,fence:None,prepare_receipt:None,store:None,custody:None,workspace:None,repository:None,head:None,active_fence:None,active_record:None,transfer_attempted:false,promoted:false,
            attempted:false,journal_attempted:false,project_attempted:false,prepare_attempted:false,acknowledged:false});out.as_mut().unwrap().initialize_retaining()
    }
    fn create(&mut self,parent:Arc<Directory>,name:&str)->Result<Arc<Directory>> {ensure!(self.directories.len()<8,"fresh directory profile bound exceeded");self.directories.push(NewDirectory::stage(parent,name)?);let last=self.directories.last_mut().unwrap();last.create_retaining(&self.original,&self.cancelled)?;last.ready()}
    fn initialize_retaining(&mut self)->Result<()> {
        ensure!(!self.attempted,"original fresh initialization already attempted");self.attempted=true;time(&self.original,&self.cancelled)?;self.preparation.assert_current()?;
        self.parent=Some(Directory::open_existing_root("/workspace/fresh-v4".into())?);let id=self.preparation.prepare().namespace.project_id.clone();let base=self.create(self.parent.as_ref().unwrap().clone(),&id)?;
        let repository=self.create(base.clone(),"repository")?;let home=self.create(base.clone(),"home")?;self.create(base,"endpoints")?;
        let rudder=self.create(repository.clone(),".rudder")?;let engine=self.create(rudder,"engine")?;let fabric=self.create(home,"fabric")?;
        time(&self.original,&self.cancelled)?;self.lock_file=Some(OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(engine.path.join("owner.lock"))?);
        self.lock_file.as_ref().unwrap().try_lock_exclusive()?;self.lock=Some(Arc::new(CheckoutLock(self.lock_file.take().unwrap())));engine.file.sync_all()?;
        // Refusal evidence only, not authority. A later legacy/default native
        // using another HOME/hash must not silently adopt this logical checkout.
        self.marker_attempted=true;self.marker=Some(OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(engine.path.join("fresh-project.json"))?);
        let bytes=rudder_engine::protocol::canonical_json(&serde_json::json!({"format_version":1,"project_id":id,"prepare_id":self.preparation.prepare().request_id,"engine_instance_id":self.preparation.registration().engine_id()}))?;
        use std::io::Write as _;self.marker.as_mut().unwrap().write_all(&bytes)?;self.marker.as_ref().unwrap().sync_all()?;engine.file.sync_all()?;self.marker_ack=true;time(&self.original,&self.cancelled)?;
        // Actual newly-created jj repository only. The source-only commit has
        // an explicit empty tree and no backend/model/profile/default task.
        let settings=self.environment.settings()?;initialize_empty_git_retaining(&mut self.workspace,&mut self.repository,&mut self.head,&repository.path,&settings,&self.original,&self.cancelled)?;
        // Journal initialization has its own staged intent in THIS original new
        // private directory. Never independently raw-open a live SQLite inode.
        self.journal_attempted=true;self.journal=Some(Journal::open(&fabric.path.join("journal.sqlite"))?);self.held=Some(Arc::new(self.journal.as_ref().unwrap().hold_database_file()?));
        self.project_attempted=true;self.fence=Some(self.journal.as_mut().unwrap().create_project(&id,&format!("fresh-genesis:{}",self.preparation.registration().engine_id()),&self.head.as_ref().unwrap().0,epoch_ms())?);
        // Stage original known ACK before postchecks. This local genesis fence
        // is deliberately never exposed as a TaskProject or remote OwnerGrant.
        let ns=self.preparation.prepare().namespace.clone();let meta=fs::symlink_metadata(&ns.journal)?;
        self.custody=Some(Arc::new(FreshNamespaceCustody{preparation:self.preparation.clone(),machine:self.environment.machine().clone(),directories:self.directories.iter().map(NewDirectory::ready).collect::<Result<_>>()?,database:self.held.as_ref().unwrap().clone(),database_id:(meta.dev(),meta.ino()),lock:self.lock.as_ref().unwrap().clone(),namespace:ns,floor_attempted:AtomicBool::new(false),floor_ack:std::sync::OnceLock::new()}));
        self.check(&self.original,&self.cancelled)?;self.prepare_attempted=true;
        crate::managed_store::prepare_fresh_retaining(self.journal.as_mut().unwrap(),self.fence.as_ref().unwrap(),self.preparation.prepare(),self.custody.as_ref().unwrap().clone(),epoch_ms(),&mut self.prepare_receipt,&mut self.store)?;
        self.check(&self.original,&self.cancelled)?;self.acknowledged=true;Ok(())
    }
    pub(crate) fn check(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        time(original,cancelled)?;self.preparation.assert_current()?;self.environment.current(original,cancelled)?;if let Some(parent)=&self.parent{parent.current()?;}for d in &self.directories{if d.ack{d.ready()?;}}
        if let Some(custody)=&self.custody{custody.check_journal(self.journal.as_ref().context("original fresh Journal absent")?)?;}
        if let Some(fence)=&self.fence{let state=self.journal.as_ref().unwrap().project_state(&fence.project_id)?;ensure!(state.owner_active&&state.owner==*fence&&state.integration_head==self.head.as_ref().unwrap().0,"actual source-only genesis owner/head changed");}
        if let Some(store)=&self.store{ensure!(store.reopen_current(self.journal.as_ref().unwrap())?==*store,"actual fresh managed namespace changed");}time(original,cancelled)
    }
    pub(crate) fn ready(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {self.check(original,cancelled)?;ensure!(self.acknowledged,"fresh namespace original ACK absent");Ok(())}
    pub(crate) fn head(&self)->Result<&rudder_vcs_bridge::GitCommitId>{ensure!(self.acknowledged,"fresh genesis ACK absent");self.head.as_ref().context("fresh head absent")}
    pub(crate) fn store(&self)->Result<&crate::managed_store::ManagedStore>{ensure!(self.acknowledged,"fresh genesis ACK absent");self.store.as_ref().context("fresh managed store absent")}
    pub(crate) fn original_git(&self)->Result<&Path>{self.ready(&self.original,&self.cancelled)?;Ok(self.repository.as_ref().unwrap().store().backend_impl::<GitBackend>().context("fresh Git backend absent")?.git_repo_path())}
    pub(crate) fn assert_retained_custody(&self)->Result<()> {self.preparation.assert_current()?;self.environment.assert_retained_custody()?;if let Some(parent)=&self.parent{parent.current()?;}for d in &self.directories{if d.ack{d.ready()?;}}self.custody.as_ref().context("fresh original namespace custody absent")?.check_custody()}
    pub(crate) fn promote_engine_retaining(&mut self,out:&mut Option<EngineOwnership>,authority:Arc<crate::managed_fresh_assigned::AssignedFreshAuthority>,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        time(original,cancel)?;ensure!(out.is_none()&&!self.transfer_attempted&&!self.promoted,"fresh original ACTIVE adoption already attempted/output occupied");self.ready(original,cancel)?;
        let lease=authority.lease();crate::managed_fresh_assigned::wait_original_lease(lease,original,cancel)?;ensure!(Arc::ptr_eq(lease.registration(),self.preparation.registration())&&lease.project_id()?==self.preparation.data().project_id&&lease.workspace_id()==self.preparation.data().workspace_id&&lease.owner_instance()==self.preparation.registration().engine_id(),"fresh assigned lease differs from original namespace");
        self.transfer_attempted=true;let owner_id=format!("fabric:{}",lease.owner_instance());
        self.active_fence=Some(self.journal.as_mut().unwrap().transfer_owner(self.fence.as_ref().unwrap(),&owner_id,epoch_ms())?);
        let binding=self.preparation.prepare().namespace.clone();let fence=self.active_fence.as_ref().unwrap().clone();
        self.active_record=Some(OwnerRecord{format_version:ENGINE_FORMAT,repo:binding.repository.clone(),authority:"fabric".into(),implementation:"managed-fresh-native-v1".into(),pid:std::process::id(),owner:RecordedFence::from(&fence),instance_id:self.preparation.registration().engine_id().into(),attach_socket:binding.repository.join(".rudder").join(ATTACH_SOCKET_NAME),control_socket:None,state:"starting".into(),started_at:now_stamp(),control_nonce:None});
        let record_path=binding.repository.join(".rudder/engine/owner.json");write_owner_record(&record_path,self.active_record.as_ref().unwrap())?;
        self.custody.as_ref().unwrap().check_journal(self.journal.as_ref().unwrap())?;let state=self.journal.as_ref().unwrap().project_state(&fence.project_id)?;
        ensure!(state.owner_active&&state.owner==fence&&state.integration_head==self.head.as_ref().unwrap().0,"fresh original local transfer ACK changed");crate::managed_fresh_assigned::wait_original_lease(lease,original,cancel)?;time(original,cancel)?;
        let record=self.active_record.as_ref().unwrap().clone();let lifetime=Arc::new(EngineOwnerLifetime{lock:self.lock.as_ref().unwrap().clone(),assigned:Some(authority),repo:record.repo.clone(),owner:fence.clone(),authority:NativeAuthority::Fabric,process_id:std::process::id(),revoked:AtomicBool::new(false)});
        // Pure move of the ORIGINAL connection/lock/receipt after known ACKs.
        // No existing project/path/serialized owner can reach this constructor.
        *out=Some(EngineOwnership{lifetime,journal:self.journal.take().unwrap(),fence,record,record_path,released:false,identity_home:binding.home,managed_store:self.store.clone()});self.promoted=true;Ok(())
    }
    pub(crate) fn check_assigned_writer(&self,journal:&Journal,expected:&OwnerFence)->Result<()> {ensure!(self.promoted&&self.active_fence.as_ref()==Some(expected),"fresh original assigned local fence differs");self.assert_retained_custody()?;self.custody.as_ref().unwrap().check_journal(journal)}
    pub(crate) fn create_integration_directory(&mut self)->Result<PathBuf>{self.ready(&self.original,&self.cancelled)?;let ns=self.store()?.binding();let parent=Directory::open_existing_root(ns.directory.clone())?;let dir=self.create(parent,"integration")?;Ok(dir.path.clone())}
}
// Reusable DATA operation for actual new private jj directories; neither this
// function nor its tests creates a Root reservation, Namespace or Engine owner.
fn initialize_empty_git_retaining(workspace_out:&mut Option<Workspace>,repo_out:&mut Option<Arc<ReadonlyRepo>>,head_out:&mut Option<rudder_vcs_bridge::GitCommitId>,directory:&Path,settings:&jj_lib::settings::UserSettings,original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
    ensure!(workspace_out.is_none()&&repo_out.is_none()&&head_out.is_none(),"original empty Git creation already staged");time(original,cancel)?;
    let (workspace,repo)=pollster::block_on(Workspace::init_internal_git(settings,directory))?;*workspace_out=Some(workspace);*repo_out=Some(repo);time(original,cancel)?;
    let repo=repo_out.as_ref().unwrap();let mut tx=repo.start_transaction();let commit=pollster::block_on(tx.repo_mut().new_commit(vec![repo.store().root_commit_id().clone()],repo.store().empty_merged_tree()).set_description("Initialize fresh Rudder project").write())?;
    let head=rudder_vcs_bridge::GitCommitId(commit.id().hex());head.validate()?;*head_out=Some(head);tx.repo_mut().set_wc_commit(WorkspaceName::DEFAULT.to_owned(),commit.id().clone())?;
    *repo_out=Some(pollster::block_on(tx.commit("initialize source-only fresh project"))?);let repo=repo_out.as_ref().unwrap();
    let mut wc=workspace_out.as_ref().unwrap().working_copy().start_mutation()?;pollster::block_on(wc.reset(&commit))?;pollster::block_on(wc.finish(repo.op_id().clone()))?;
    let git=repo.store().backend_impl::<GitBackend>().context("fresh jj is not internal Git")?.git_repo_path();let mut file=OpenOptions::new().write(true).truncate(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(git.join("HEAD"))?;
    use std::io::Write as _;file.write_all(format!("{}\n",head_out.as_ref().unwrap().0).as_bytes())?;file.sync_all()?;File::open(git)?.sync_all()?;time(original,cancel)
}
fn time(original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {if cancel.load(Ordering::Acquire){original.cancel();anyhow::bail!("original fresh initialization cancelled");}original.remaining()?;Ok(())}

#[cfg(test)]
#[path="engine_host_fresh_tests.rs"]
mod tests;
