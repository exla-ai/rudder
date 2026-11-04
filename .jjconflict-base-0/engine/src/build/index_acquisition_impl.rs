use super::*;
use crate::build::index_acquisition::{RetainedIndexAcquisition,IndexAcquisitionPhase};
use std::{ffi::CString,os::fd::{AsRawFd,FromRawFd},os::unix::fs::PermissionsExt};

impl BuildIndex {
    /// Existing actual private parent only; the new child MUST be absent.
    /// Root/native caller supplies the same already-captured original budget.
    pub fn create_shared_retaining(out:&mut Option<RetainedIndexAcquisition>,root:PathBuf,store:LocalStore,
        original:FinalizationBudget,cancelled:Arc<AtomicBool>,restriction:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original shared index acquisition occupied");
        *out=Some(RetainedIndexAcquisition{requested:root,store,original,cancelled,restriction,phase:IndexAcquisitionPhase::Captured,
            parent:None,root:None,locks:None,parent_id:None,root_id:None,locks_id:None,mkdir_attempted:false,sqlite_attempted:false,sql_step:0,
            connection:None,index:None,failure:None});let owner=out.as_mut().unwrap();
        if let Err(error)=owner.run(){owner.phase=IndexAcquisitionPhase::Failed;owner.failure=Some(error);anyhow::bail!("shared index acquisition failed; retain original owner")}
        Ok(())
    }
}
fn id(m:&Metadata)->(u64,u64){(m.dev(),m.ino())}
fn private_dir(file:&File)->Result<(u64,u64)>{let m=file.metadata()?;
    ensure!(m.is_dir()&&m.uid()==unsafe{libc::geteuid()}&&m.gid()==unsafe{libc::getegid()}&&m.mode()&0o7777==0o700,
        "shared index directory is not original private owner");Ok(id(&m))}
fn open_child(parent:&File,name:&str)->Result<File>{let name=CString::new(name)?;
    let fd=unsafe{libc::openat(parent.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC)};
    ensure!(fd>=0,"original shared index child open failed");Ok(unsafe{File::from_raw_fd(fd)})}
impl RetainedIndexAcquisition {
    fn time(&self)->Result<()> {if self.cancelled.load(Ordering::Acquire){self.original.cancel();}self.original.remaining()?;self.restriction.remaining()?;self.original.remaining()?;Ok(())}
    fn check(&self)->Result<()> {
        self.time()?;let parent=self.requested.parent().context("shared index parent absent")?;
        if let (Some(file),Some(expected))=(&self.parent,self.parent_id){let m=fs::symlink_metadata(parent)?;
            ensure!(private_dir(file)?==expected&&id(&m)==expected&&m.is_dir()&&!m.file_type().is_symlink(),"original shared index parent changed");}
        if let (Some(file),Some(expected))=(&self.root,self.root_id){let m=fs::symlink_metadata(&self.requested)?;
            ensure!(private_dir(file)?==expected&&id(&m)==expected&&m.is_dir()&&!m.file_type().is_symlink(),"original shared index root changed");}
        if let (Some(file),Some(expected))=(&self.locks,self.locks_id){let m=fs::symlink_metadata(self.requested.join("locks"))?;
            ensure!(private_dir(file)?==expected&&id(&m)==expected&&m.is_dir()&&!m.file_type().is_symlink(),"original shared index locks changed");}
        if let Some(connection)=&self.connection{sqlite_binding_current(connection,&self.requested.join("index.sqlite"))?;}
        if let Some(index)=&self.index{loop{self.time()?;match index.connection.try_lock(){
            Ok(keeper)=>{sqlite_binding_current(&keeper,&index.root.join("index.sqlite"))?;break;},
            Err(TryLockError::WouldBlock)=>std::thread::sleep(self.original.remaining()?.min(Duration::from_millis(1))),
            Err(_)=>anyhow::bail!("original shared index keeper poisoned")}}}
        self.time()
    }
    fn run(&mut self)->Result<()> {
        self.time()?;ensure!(rusqlite::version_number()>=3_051_003,"SQLite >=3.51.3 required for shared WAL safety");
        ensure!(self.requested.is_absolute()&&self.requested.file_name().is_some(),"shared index path must be explicit absolute child");
        let parent=self.requested.parent().unwrap();ensure!(parent.canonicalize()?==parent,"shared index parent redirects");
        self.parent=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(parent)?);
        self.parent_id=Some(private_dir(self.parent.as_ref().unwrap())?);self.check()?;
        let name=CString::new(self.requested.file_name().unwrap().as_encoded_bytes())?;
        self.phase=IndexAcquisitionPhase::Creating;self.mkdir_attempted=true;
        ensure!(unsafe{libc::mkdirat(self.parent.as_ref().unwrap().as_raw_fd(),name.as_ptr(),0o700)}==0,
            "shared index child exists/original creation unknown; explicit recovery required");
        self.root=Some(open_child(self.parent.as_ref().unwrap(),self.requested.file_name().unwrap().to_str().context("index name not UTF8")?)?);
        self.root_id=Some(private_dir(self.root.as_ref().unwrap())?);self.requested=self.requested.canonicalize()?;self.check()?;
        ensure!(unsafe{libc::mkdirat(self.root.as_ref().unwrap().as_raw_fd(),c"locks".as_ptr(),0o700)}==0,"shared index locks creation unknown");
        self.locks=Some(open_child(self.root.as_ref().unwrap(),"locks")?);self.locks_id=Some(private_dir(self.locks.as_ref().unwrap())?);self.check()?;
        self.phase=IndexAcquisitionPhase::Opening;self.sqlite_attempted=true;
        self.connection=Some(Connection::open_with_flags(self.requested.join("index.sqlite"),rusqlite::OpenFlags::SQLITE_OPEN_CREATE|rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            |rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX|rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW|rusqlite::OpenFlags::SQLITE_OPEN_PRIVATE_CACHE)?);
        self.connection.as_ref().unwrap().busy_timeout(Duration::ZERO)?;
        let original=self.original.clone();let cancel=self.cancelled.clone();let restriction=self.restriction.clone();
        self.connection.as_mut().unwrap().progress_handler(1000,Some(move||{if cancel.load(Ordering::Acquire){original.cancel();}original.remaining().is_err()||restriction.remaining().is_err()}))?;
        fs::set_permissions(self.requested.join("index.sqlite"),fs::Permissions::from_mode(0o600))?;self.check()?;
        self.phase=IndexAcquisitionPhase::Configuring;
        for (i,statement) in ["PRAGMA journal_mode=WAL","PRAGMA synchronous=FULL",
            "CREATE TABLE build_index(format_version INTEGER NOT NULL,category TEXT NOT NULL,key_digest TEXT NOT NULL,key_json TEXT NOT NULL,receipt_json TEXT NOT NULL,PRIMARY KEY(category,key_digest))"].into_iter().enumerate(){
            self.check()?;self.sql_step=i+1;self.connection.as_ref().unwrap().execute_batch(statement)?;self.check()?;
        }
        let connection=self.connection.as_ref().unwrap();let wal:String=connection.query_row("PRAGMA journal_mode",[],|r|r.get(0))?;
        let sync:i64=connection.query_row("PRAGMA synchronous",[],|r|r.get(0))?;
        ensure!(wal.eq_ignore_ascii_case("wal")&&sync==2&&connection.is_autocommit(),"original shared index WAL/FULL/schema ACK absent");self.check()?;
        for file in [self.locks.as_ref().unwrap(),self.root.as_ref().unwrap(),self.parent.as_ref().unwrap()]{self.time()?;file.sync_all()?;self.time()?;}
        // The keeper outlives this completed acquisition. A later legitimate
        // preparation supplies its own original clock; do not leave an expired
        // acquisition callback installed on that shared SQLite connection.
        self.connection.as_mut().unwrap().progress_handler(0,None::<fn()->bool>)?;
        self.index=Some(Arc::new(BuildIndex{store:self.store.clone(),connection:std::sync::Mutex::new(self.connection.take().unwrap()),
            locks:self.requested.join("locks"),root:self.requested.clone()}));
        self.check()?;self.phase=IndexAcquisitionPhase::Acknowledged;Ok(())
    }
}

#[cfg(test)]#[path="index_acquisition_tests.rs"]mod tests;
