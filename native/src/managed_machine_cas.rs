//! One actual shared LocalStore for the isolated machine/account/home/store.
//! Configuration is borrowed only from the genuine Root capability, and the
//! registered engine is physical custody, not project/task/source permission.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use rudder_engine::{execution::finalization_budget::FinalizationBudget,store::LocalStore};
use rudder_native::{host_ingress::{HostIngressConfigOwner,HostReceiptStorePolicy},engine_registration::RegisteredEngineOwner};
use std::{fs::{self,File,OpenOptions},os::unix::fs::{MetadataExt,OpenOptionsExt},path::PathBuf,sync::{Arc,atomic::{AtomicBool,Ordering}}};
struct Directory {path:PathBuf,file:File,id:(u64,u64)}
impl Directory {
    fn capture(path:PathBuf,uid:u32,gid:u32,mode:u32)->Result<Self> {ensure!(path.canonicalize()?==path,"machine CAS path redirects");let file=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&path)?;let m=file.metadata()?;let owner=Self{path,file,id:(m.dev(),m.ino())};owner.check(uid,gid,mode)?;Ok(owner)}
    fn check(&self,uid:u32,gid:u32,mode:u32)->Result<()> {for m in [self.file.metadata()?,fs::symlink_metadata(&self.path)?]{ensure!(m.is_dir()&&!m.file_type().is_symlink()&&(m.dev(),m.ino())==self.id&&m.uid()==uid&&m.gid()==gid&&m.mode()&0o7777==mode,"actual machine CAS directory changed");}Ok(())}
}
/// Keep SAME mutable carrier outside catch until open returns. Partial directory
/// initialization and the actual LocalStore remain retained on every failure.
pub(crate) struct ManagedMachineCas {store:Option<LocalStore>,workspace:Option<Directory>,state:Option<Directory>,cas:Option<Directory>,
    config:Arc<HostIngressConfigOwner>,registration:Arc<RegisteredEngineOwner>,attempted:bool,acknowledged:bool,fenced:AtomicBool}
impl ManagedMachineCas {
    pub(crate) fn capture_retaining(out:&mut Option<Self>,config:Arc<HostIngressConfigOwner>,registration:Arc<RegisteredEngineOwner>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        time(original,cancelled)?;config.assert_current()?;registration.assert_current()?;ensure!(out.is_none(),"actual machine CAS already captured");
        let d=config.data();ensure!(d.account_id==registration.account_id()&&d.home_id==registration.home_id()&&d.machine_id==registration.machine_id()&&d.incarnation==registration.incarnation()&&registration.data().binding.config_sha256==config.config_sha256(),"shared CAS differs from original registered policy");
        let expected=d.state_directory.join("cas");ensure!(d.projects.iter().all(|p|p.cas==expected),"every fresh project must declare SAME actual machine CAS");
        ensure!(matches!(d.store_policy,HostReceiptStorePolicy::InitializeEmptyV1),"existing machine CAS needs retained recovery; no JSON restart adoption");
        *out=Some(Self{store:None,workspace:None,state:None,cas:None,config,registration,attempted:false,acknowledged:false,fenced:AtomicBool::new(false)});out.as_mut().unwrap().open_retaining(original,cancelled)
    }
    fn open_retaining(&mut self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        ensure!(!self.attempted,"original machine CAS open already attempted");self.attempted=true;time(original,cancelled)?;
        self.workspace=Some(Directory::capture("/workspace".into(),0,0,0o755)?);
        self.state=Some(Directory::capture(self.config.data().state_directory.clone(),1500,1500,0o700)?);
        self.cas=Some(Directory::capture(self.config.data().state_directory.join("cas"),1500,1500,0o700)?);
        ensure!(fs::read_dir(&self.cas.as_ref().unwrap().path)?.next().is_none(),"fresh machine CAS already has data; original recovery required");
        self.check_physical(original,cancelled)?;self.store=Some(LocalStore::open(&self.cas.as_ref().unwrap().path)?);
        self.check_physical(original,cancelled)?;self.acknowledged=true;Ok(())
    }
    fn check_physical(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        time(original,cancelled)?;self.config.assert_current()?;self.registration.assert_current()?;ensure!(!self.fenced.load(Ordering::Acquire),"actual machine CAS fenced");
        self.workspace.as_ref().context("actual workspace absent")?.check(0,0,0o755)?;self.state.as_ref().context("actual state absent")?.check(1500,1500,0o700)?;self.cas.as_ref().context("actual CAS absent")?.check(1500,1500,0o700)?;time(original,cancelled)
    }
    pub(crate) fn assert_custody(&self)->Result<()> {
        ensure!(self.acknowledged&&!self.fenced.load(Ordering::Acquire),"actual machine CAS unavailable");self.config.assert_current()?;self.registration.assert_current()?;
        self.workspace.as_ref().context("workspace absent")?.check(0,0,0o755)?;self.state.as_ref().context("state absent")?.check(1500,1500,0o700)?;self.cas.as_ref().context("CAS absent")?.check(1500,1500,0o700)
    }
    pub(crate) fn assert_current(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        let result=(||{ensure!(self.acknowledged,"original shared CAS open ACK absent");self.check_physical(original,cancelled)})();if result.is_err(){self.fenced.store(true,Ordering::Release);}result
    }
    pub(crate) fn store(&self)->Result<&LocalStore> {ensure!(self.acknowledged&&!self.fenced.load(Ordering::Acquire),"actual shared CAS open ACK unavailable");self.store.as_ref().context("actual shared CAS absent")}
    pub(crate) fn config(&self)->&Arc<HostIngressConfigOwner>{&self.config}
    pub(crate) fn registration(&self)->&Arc<RegisteredEngineOwner>{&self.registration}
}
fn time(original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("machine CAS original initialization cancelled");}original.remaining()?;Ok(())}
