//! Narrow startup protection of actual provider-injected originals. This is
//! neither config authentication nor a Root/image/registration grant. The
//! actual MachineRootBootstrap owns this carrier before any mutating syscall.
//! Existing parsers still require exactly0600. No content reads, path override,
//! recursive repair, cleanup, retry or I/O in Drop.
use anyhow::{ensure,Context,Result};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use std::{ffi::CString,fs::{self,File,OpenOptions},io,os::{fd::{AsRawFd,FromRawFd},unix::fs::OpenOptionsExt},path::Path};
const DIRECTORY:&str="/run/rudder-host-control";
#[derive(Clone,Copy)]
struct Spec {name:&'static str,min:u64,max:u64}
const FILES:[Spec;3]=[
    Spec {name:"config.json",min:1,max:super::host_ingress::MAX_HOST_FRAME as u64},
    Spec {name:"token",min:32,max:4096},
    Spec {name:"registration-key",min:65,max:65},
];
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
struct Stamp {dev:u64,ino:u64,len:u64,mode:u32,uid:u32,gid:u32,links:u64,mtime:(i64,i64),ctime:(i64,i64)}
impl Stamp {
    fn from_raw(s:&libc::stat)->Result<Self> {ensure!(s.st_size>=0,"host injected file length invalid");
        Ok(Self {dev:s.st_dev as u64,ino:s.st_ino as u64,len:s.st_size as u64,mode:s.st_mode as u32,
            uid:s.st_uid,gid:s.st_gid,links:s.st_nlink as u64,mtime:(s.st_mtime,s.st_mtime_nsec),ctime:(s.st_ctime,s.st_ctime_nsec)})}
    fn held(file:&File)->Result<Self> {let mut raw:libc::stat=unsafe {std::mem::zeroed()};
        if unsafe {libc::fstat(file.as_raw_fd(),&mut raw)}!=0 {return Err(io::Error::last_os_error().into());}Self::from_raw(&raw)}
    fn named(parent:i32,name:&CString)->Result<Self> {let mut raw:libc::stat=unsafe {std::mem::zeroed()};
        if unsafe {libc::fstatat(parent,name.as_ptr(),&mut raw,libc::AT_SYMLINK_NOFOLLOW)}!=0 {return Err(io::Error::last_os_error().into());}Self::from_raw(&raw)}
    fn regular(&self,spec:Spec,owner:(u32,u32))->Result<()> {
        ensure!(self.mode&libc::S_IFMT==libc::S_IFREG && (self.uid,self.gid)==owner && self.links==1
            && (spec.min..=spec.max).contains(&self.len),"host injected file ownership/type/size invalid");
        ensure!(matches!(self.mode&0o7777,0o600|0o755),"host injected file mode unsupported");Ok(())
    }
    fn same_payload_metadata(&self,next:&Self)->bool {
        (self.dev,self.ino,self.len,self.uid,self.gid,self.links,self.mtime,self.mode&libc::S_IFMT)
            ==(next.dev,next.ino,next.len,next.uid,next.gid,next.links,next.mtime,next.mode&libc::S_IFMT)
    }
}
struct Directory {file:File,name:CString,parent:Option<usize>,stamp:Option<Stamp>,owner:(u32,u32),private:bool}
struct OriginalFile {file:File,spec:Spec,name:CString,before:Option<Stamp>,after:Option<Stamp>,chmod_attempted:bool,sync_attempted:bool}
#[derive(Clone,Copy,PartialEq,Eq)]
enum Phase {Capturing,Normalizing,Ready,Absent}
#[must_use="retain original provisioning file/directory FDs after every uncertain startup effect"]
pub(super) struct ProvisionedFileModes {
    original:FinalizationBudget,directories:Vec<Directory>,files:Vec<OriginalFile>,owner:(u32,u32),phase:Phase,
    #[cfg(test)] after_chmod:Option<fn(&FinalizationBudget)>,
}
impl ProvisionedFileModes {
    fn empty(original:&FinalizationBudget,owner:(u32,u32))->Self {
        Self {original:original.clone(),directories:Vec::with_capacity(3),files:Vec::with_capacity(3),owner,phase:Phase::Capturing,#[cfg(test)] after_chmod:None}
    }
    /// Called only after the actual launcher's Root/global namespace/image
    /// checks, before RootHostControl/RootKey parse or any child/volume effects.
    pub(super) fn normalize_retaining(out:&mut Option<Self>,original:&FinalizationBudget)->Result<()> {
        ensure!(out.is_none(),"host injected file normalization already attempted");
        *out=Some(Self::empty(original,(0,0)));let owner=out.as_mut().unwrap();owner.original.remaining()?;
        ensure!(unsafe {libc::getuid()}==0 && unsafe {libc::geteuid()}==0 && unsafe {libc::getgid()}==0
            && unsafe {libc::getegid()}==0,"host injected files require actual Root");
        if owner.absent_or_complete(Path::new(DIRECTORY))? {owner.original.remaining()?;owner.phase=Phase::Absent;return Ok(());}
        owner.open_directory(None,"/",false)?;owner.open_directory(Some(0),"run",false)?;
        owner.open_directory(Some(1),"rudder-host-control",true)?;owner.capture_and_normalize()
    }
    fn absent_or_complete(&self,path:&Path)->Result<bool> {
        let mut count=0;for spec in FILES {self.original.remaining()?;match fs::symlink_metadata(path.join(spec.name)) {
            Ok(_)=>count+=1,Err(error)if error.kind()==io::ErrorKind::NotFound=>{},Err(error)=>return Err(error.into()),}}
        ensure!(count==0 || count==FILES.len(),"host optional protected file set incomplete");Ok(count==0)
    }
    fn open_directory(&mut self,parent:Option<usize>,name:&str,private:bool)->Result<()> {
        self.original.remaining()?;let name=CString::new(name)?;
        let file=if let Some(index)=parent {open_at(&self.directories[index].file,&name,true)?}
            else {OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(Path::new(name.to_str()?))?};
        // Retain before every metadata/post-open identity check.
        self.directories.push(Directory {file,name,parent,stamp:None,owner:self.owner,private});
        let index=self.directories.len()-1;let stamp=Stamp::held(&self.directories[index].file)?;
        self.directories[index].stamp=Some(stamp);self.check_directories()
    }
    fn check_directories(&self)->Result<()> {
        for directory in &self.directories {let actual=Stamp::held(&directory.file)?;let captured=directory.stamp.context("host protected parent capture incomplete")?;
            let parent=directory.parent.map_or(libc::AT_FDCWD,|i|self.directories[i].file.as_raw_fd());
            let named=Stamp::named(parent,&directory.name)?;
            ensure!(actual.mode&libc::S_IFMT==libc::S_IFDIR && (actual.uid,actual.gid)==directory.owner
                && actual.mode&0o022==0 && (!directory.private || actual.mode&0o7777==0o700),"host protected parent invalid");
            ensure!((actual.dev,actual.ino,actual.mode,actual.uid,actual.gid)==(captured.dev,captured.ino,captured.mode,captured.uid,captured.gid)
                && (actual.dev,actual.ino,actual.mode,actual.uid,actual.gid)==(named.dev,named.ino,named.mode,named.uid,named.gid),"host original protected parent replaced");}
        Ok(())
    }
    fn capture_and_normalize(&mut self)->Result<()> {
        self.check_directories()?;let directory=&self.directories.last().context("host protected parent absent")?.file;
        for spec in FILES {self.original.remaining()?;let name=CString::new(spec.name)?;let file=open_at(directory,&name,false)?;
            self.files.push(OriginalFile {file,spec,name,before:None,after:None,chmod_attempted:false,sync_attempted:false});
            let slot=self.files.last_mut().unwrap();let before=Stamp::held(&slot.file)?;slot.before=Some(before);before.regular(spec,self.owner)?;
            ensure!(before==Stamp::named(directory.as_raw_fd(),&slot.name)?,"host injected original file changed while opening");}
        // Validate ALL3 complete actual originals before the first chmod.
        self.check_initials()?;self.phase=Phase::Normalizing;
        for i in 0..self.files.len() {
            self.original.remaining()?;self.check_directories()?;let directory=self.directories.last().unwrap().file.as_raw_fd();
            let slot=&mut self.files[i];let before=slot.before.unwrap();
            ensure!(before==Stamp::held(&slot.file)? && before==Stamp::named(directory,&slot.name)?,"host injected original changed before protection");
            if before.mode&0o7777==0o755 {
                slot.chmod_attempted=true;
                if unsafe {libc::fchmod(slot.file.as_raw_fd(),0o600)}!=0 {return Err(io::Error::last_os_error().into());}
                #[cfg(test)] if let Some(hook)=self.after_chmod {hook(&self.original);}
                self.original.remaining()?;slot.sync_attempted=true;slot.file.sync_all()?;self.original.remaining()?;
            }
            let after=Stamp::held(&slot.file)?;after.regular(slot.spec,self.owner)?;
            ensure!(after.mode&0o7777==0o600 && before.same_payload_metadata(&after)
                && (slot.chmod_attempted || before==after) && after==Stamp::named(directory,&slot.name)?,"host injected file protection/currentness uncertain");
            slot.after=Some(after);
        }
        self.check_finished()?;self.original.remaining()?;self.phase=Phase::Ready;Ok(())
    }
    fn check_initials(&self)->Result<()> {self.check_directories()?;let directory=self.directories.last().unwrap().file.as_raw_fd();
        for slot in &self.files {let before=slot.before.context("host injected original capture incomplete")?;before.regular(slot.spec,self.owner)?;
            ensure!(before==Stamp::held(&slot.file)? && before==Stamp::named(directory,&slot.name)?,"host injected original replaced before normalization");}Ok(())}
    fn check_finished(&self)->Result<()> {self.check_directories()?;ensure!(self.files.len()==FILES.len(),"host protected file capture incomplete");
        let directory=self.directories.last().unwrap().file.as_raw_fd();for slot in &self.files {let after=slot.after.context("host file protection not acknowledged")?;
            let actual=Stamp::held(&slot.file)?;actual.regular(slot.spec,self.owner)?;ensure!(actual.mode&0o7777==0o600 && actual==after
                && actual==Stamp::named(directory,&slot.name)?,"host retained protected file changed");}Ok(())}
    /// The original startup clock gates only the original mutations. A healthy
    /// retained0600 file is not fenced merely because startup30 later elapsed.
    pub(super) fn check_current(&self)->Result<()> {match self.phase {
        Phase::Absent=>Ok(()),Phase::Ready=>self.check_finished(),_=>anyhow::bail!("host file normalization uncertain; original owners retained"),}}
    pub(super) fn configured(&self)->Result<bool> {self.check_current()?;Ok(self.phase==Phase::Ready)}
}
fn open_at(parent:&File,name:&CString,directory:bool)->Result<File> {
    let flags=libc::O_RDONLY|libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK|if directory {libc::O_DIRECTORY}else{0};
    let raw=unsafe {libc::openat(parent.as_raw_fd(),name.as_ptr(),flags)};
    if raw<0 {return Err(io::Error::last_os_error().into());}Ok(unsafe {File::from_raw_fd(raw)})
}
#[cfg(test)]
#[path="root_host_control_file_modes_tests.rs"]
mod tests;
