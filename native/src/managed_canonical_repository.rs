//! Original integration writer acquisition and settlement. The linear
//! publication permit is private to the actual canonical completion consumer.
use super::*;

struct AcquirePermit;
enum Acquisition { Unattempted(AcquirePermit), Attempted, Owned, Busy, Released }
pub(in crate::managed_checkpoint_accept) struct RetainedIntegrationOpen {
    path:PathBuf, directory:Option<File>, lock:Option<File>, loader:Option<RepoLoader>,
    directories:Vec<IntegrationDirectory>, acquisition:Acquisition, attempted:bool,
}
impl RetainedIntegrationOpen {
    pub(in crate::managed_checkpoint_accept) fn open_retaining(out:&mut Option<Self>, ready:&mut Option<PrivateIntegration>,path:PathBuf,name:&str,email:&str,
        original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
        ensure!(out.is_none()&&ready.is_none(),"original integration acquisition already occupied");
        *out=Some(Self{path,directory:None,lock:None,loader:None,directories:Vec::new(),acquisition:Acquisition::Unattempted(AcquirePermit),attempted:false});
        let pending=out.as_mut().unwrap();ensure!(!pending.attempted,"integration acquisition cannot replay");pending.attempted=true;
        time(original,cancel)?;ensure!(pending.path.is_absolute()&&pending.path.canonicalize()?==pending.path,"integration requires exact existing canonical namespace");
        let meta=fs::symlink_metadata(&pending.path)?;ensure!(meta.is_dir()&&meta.uid()==unsafe{libc::geteuid()}&&meta.mode()&0o777==0o700,"integration namespace is not original private owner");
        pending.directory=Some(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_DIRECTORY).open(&pending.path)?);
        pending.lock=Some(OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(pending.path.join("rudder-acceptance.lock"))?);
        let lock=pending.lock.as_ref().unwrap();let meta=lock.metadata()?;
        ensure!(meta.is_file()&&meta.uid()==unsafe{libc::geteuid()}&&meta.mode()&0o777==0o600,"integration lock invalid");
        time(original,cancel)?;
        // Consume BEFORE the actual acquisition. No absent FD/path or later
        // observation can recreate the no-acquisition proof after uncertainty.
        ensure!(matches!(pending.acquisition,Acquisition::Unattempted(_)),"original acquire permit absent");pending.acquisition=Acquisition::Attempted;
        match lock.try_lock_exclusive(){Ok(())=>pending.acquisition=Acquisition::Owned,Err(e)if e.kind()==std::io::ErrorKind::WouldBlock=>{pending.acquisition=Acquisition::Busy;return Err(e).context("canonical integration writer already owned")},Err(e)=>return Err(e).context("original integration acquire outcome uncertain")}
        pending.directories.try_reserve_exact(9)?;
        for relative in [".jj",".jj/repo",".jj/repo/store",".jj/repo/op_store",".jj/repo/op_heads",".jj/repo/index",".jj/repo/submodule_store",".jj/repo/store/git",".jj/repo/store/git/objects"] {
            time(original,cancel)?;let path=pending.path.join(relative);ensure!(path.canonicalize()?==path,"symlinked integration hierarchy unsupported");
            let held=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_DIRECTORY|libc::O_NONBLOCK).open(&path)?;
            pending.directories.push(IntegrationDirectory{path,held});pending.directories.last().unwrap().check_current()?;
        }
        pending.loader=Some(RepoLoader::init_from_file_system(&settings(name,email)?,&pending.path.join(".jj/repo"),&StoreFactories::default())?);
        let git=pending.loader.as_ref().unwrap().store().backend_impl::<GitBackend>().context("integration needs pinned Git backend")?;
        let expected=pending.path.join(".jj/repo/store/git");ensure!(git.git_repo_path()==expected.as_path()&&expected.canonicalize()?==expected,"integration Git store changed");
        // Move actual originals into the caller's owning slot BEFORE final
        // currentness/clock checks. The partial carrier also stays retained.
        *ready=Some(PrivateIntegration{loader:pending.loader.take().unwrap(),directories:std::mem::take(&mut pending.directories),directory:pending.directory.take().unwrap(),
            lock:pending.lock.take().unwrap(),path:pending.path.clone(),lock_held:true,writer_released:Arc::new(AtomicBool::new(false))});
        pending.acquisition=Acquisition::Released;ready.as_ref().unwrap().check_current()?;time(original,cancel)
    }
}

#[cfg(test)]
#[path="managed_canonical_repository_tests.rs"]mod tests;

/// ACK of only an actual original writer unlock, or an unconsumed actual
/// acquisition permit/known non-acquisition. NOT task, SDK, helper or TLS drain.
pub(super) struct UnpublishedRelease {lock:Option<File>,path:PathBuf,attempted:bool,ack:bool,timely:bool}
impl UnpublishedRelease {pub(super) fn acknowledged(&self)->bool{self.ack&&self.timely}}
fn unlock(original:&File,path:&std::path::Path,release:&mut UnpublishedRelease,clock:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
    time(clock,cancel)?;let held=original.metadata()?;let named=fs::symlink_metadata(path.join("rudder-acceptance.lock"))?;
    ensure!(held.is_file()&&(held.dev(),held.ino(),held.uid(),held.mode())==(named.dev(),named.ino(),named.uid(),named.mode()),"original writer lock changed");
    release.lock=Some(original.try_clone()?);release.path=path.to_path_buf();release.attempted=true;
    FileExt::unlock(original)?;release.ack=true;Ok(())
}
pub(super) fn release_unpublished_retaining(out:&mut Option<UnpublishedRelease>,acceptor:&mut ManagedCheckpointAcceptance,_permit:&PublicationPermit,
    original:&FinalizationBudget,cancel:&AtomicBool)->Result<()> {
    ensure!(out.is_none(),"original unpublished release cannot replay");
    *out=Some(UnpublishedRelease{lock:None,path:PathBuf::new(),attempted:false,ack:false,timely:false});let release=out.as_mut().unwrap();
    time(original,cancel)?;acceptor.lease.check_current()?;
    ensure!(!acceptor.preparation_attempted&&!acceptor.publish_attempted&&acceptor.request.is_none()&&acceptor.intent.is_none(),"canonical publication effects were attempted");
    if let Some(repository)=&mut acceptor.repository {
        repository.check_current()?;unlock(&repository.lock,&repository.path,release,original,cancel)?;repository.lock_held=false;
    }else if let Some(pending)=&mut acceptor.repository_open {
        match pending.acquisition {
            Acquisition::Unattempted(_)|Acquisition::Busy=>{pending.acquisition=Acquisition::Released;release.ack=true;},
            Acquisition::Owned=>{unlock(pending.lock.as_ref().context("original acquired lock absent")?,&pending.path,release,original,cancel)?;pending.acquisition=Acquisition::Released;},
            _=>anyhow::bail!("integration acquire uncertainty retains original lock and serial slot"),
        }
    }else {
        // This permit was born in this original completion consumer before
        // any repository call; no acquisition was dispatched from it.
        release.ack=true;
    }
    time(original,cancel)?;release.timely=true;Ok(())
}
