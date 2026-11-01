//! Single-directory original identity custody. No ancestor recursion, authority,
//! lazy reopen, clock construction, state mutation or IO-in-Drop.
use super::Stamp;
use anyhow::{ensure,Context,Result};
use std::{collections::BTreeMap,fs::{self,File,OpenOptions},os::unix::{ffi::OsStrExt,fs::{MetadataExt,OpenOptionsExt}},path::{Component,Path,PathBuf},sync::Arc};
const MAX_ANCHORS:usize=8192;const MAX_PATH:usize=4096;const MAX_PATH_BYTES:usize=8<<20;const OTHER_FD_RESERVE:u64=128;
pub(super) struct DirectoryAnchor {held:File,pub(super) stamp:Stamp,uid:u32,gid:u32}
struct Pending {held:Option<File>,stamp:Option<Stamp>,ready:Option<Arc<DirectoryAnchor>>}
pub(super) struct DirectoryAnchors {paths:BTreeMap<PathBuf,Pending>,path_bytes:usize,uid:u32,gid:u32,limit:usize}
impl Default for DirectoryAnchors{fn default()->Self{Self{paths:BTreeMap::new(),path_bytes:0,uid:0,gid:0,limit:MAX_ANCHORS}}}
fn version(path:&Path,m:&fs::Metadata,uid:u32,gid:u32)->Result<Stamp>{ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==uid&&m.gid()==gid&&m.mode()&0o022==0,"runtime directory is not original protected ownership: {}",path.display());
    Ok(Stamp{path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec()),mode:m.mode()})}
impl DirectoryAnchor {
    pub(super) fn current(&self)->Result<()> {ensure!(version(&self.stamp.path,&self.held.metadata()?,self.uid,self.gid)?==self.stamp
        &&version(&self.stamp.path,&fs::symlink_metadata(&self.stamp.path)?,self.uid,self.gid)?==self.stamp,"original runtime directory identity/version changed: {}",self.stamp.path.display());Ok(())}
}
impl DirectoryAnchors {
    pub(super) fn capture(&mut self,path:&Path)->Result<Arc<DirectoryAnchor>> {self.capture_inner(path,||Ok(()))}
    fn capture_inner(&mut self,path:&Path,after_open:impl FnOnce()->Result<()>)->Result<Arc<DirectoryAnchor>> {
        if let Some(slot)=self.paths.get(path){let actual=slot.ready.as_ref().context("original runtime directory capture incomplete; no retry")?;actual.current()?;return Ok(actual.clone())}
        let bytes=path.as_os_str().as_bytes().len();ensure!(path.is_absolute()&&bytes<=MAX_PATH&&path.components().all(|c|matches!(c,Component::RootDir|Component::Normal(_))),"runtime directory path outside fixed bound");
        ensure!(self.paths.len()<self.limit&&self.paths.len()<MAX_ANCHORS&&self.path_bytes.checked_add(bytes).is_some_and(|n|n<=MAX_PATH_BYTES),"runtime directory anchor/path capacity exceeded");
        self.paths.insert(path.into(),Pending{held:None,stamp:None,ready:None});self.path_bytes+=bytes;
        let slot=self.paths.get_mut(path).unwrap();slot.stamp=Some(version(path,&fs::symlink_metadata(path)?,self.uid,self.gid)?);
        let mut limit=std::mem::MaybeUninit::<libc::rlimit>::uninit();ensure!(unsafe{libc::getrlimit(libc::RLIMIT_NOFILE,limit.as_mut_ptr())}==0,"runtime directory RLIMIT unavailable");
        let limit=unsafe{limit.assume_init()};ensure!(limit.rlim_cur==libc::RLIM_INFINITY||(self.paths.len()as u64).checked_add(OTHER_FD_RESERVE).is_some_and(|v|v<limit.rlim_cur as u64),"runtime directory anchors exceed process soft FD limit");
        let slot=self.paths.get_mut(path).unwrap();slot.held=Some(OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?);
        let before=slot.stamp.as_ref().unwrap();ensure!(version(path,&slot.held.as_ref().unwrap().metadata()?,self.uid,self.gid)?==*before,"runtime directory changed during original open: {}",path.display());
        after_open()?;ensure!(version(path,&fs::symlink_metadata(path)?,self.uid,self.gid)?==*before,"runtime directory changed after original open: {}",path.display());
        let actual=Arc::new(DirectoryAnchor{held:slot.held.take().unwrap(),stamp:slot.stamp.take().unwrap(),uid:self.uid,gid:self.gid});slot.ready=Some(actual.clone());Ok(actual)
    }
    pub(super) fn capture_expected(&mut self,path:&Path,before:&Stamp)->Result<Arc<DirectoryAnchor>> {let actual=self.capture(path)?;ensure!(actual.stamp==*before,"runtime directory earlier stamp differs: {}",path.display());Ok(actual)}
    pub(super) fn check_all_with(&self,mut check:impl FnMut()->Result<()>)->Result<()> {for slot in self.paths.values(){check()?;slot.ready.as_ref().context("original runtime directory capture incomplete")?.current()?;}check()}
}
#[cfg(test)]
#[path="managed_runtime_directory_anchor_tests.rs"]
mod tests;
