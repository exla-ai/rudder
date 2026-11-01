//! Fixed image directory identity DATA, not runtime/task/Root authority.
//! Retain this map in the actual ImageDataBuild through all later exports.
use super::Stamp;
use anyhow::{ensure,Context,Result};
use std::{collections::BTreeMap,fs::{self,File,OpenOptions},os::unix::{ffi::OsStrExt,fs::{MetadataExt,OpenOptionsExt}},path::{Component,Path,PathBuf}};
const MAX_DIRECTORY_PATHS:usize=8192;
const MAX_PATH_BYTES:usize=4096;
const MAX_TOTAL_PATH_BYTES:usize=8<<20;
const OTHER_FD_RESERVE:u64=128;

struct HeldDirectory {file:Option<File>,before:Option<Stamp>,complete:bool}
/// There is no IO-in-Drop, clone/serde constructor, replay or inode adoption.
pub(super) struct DirectoryCustody {held:BTreeMap<PathBuf,HeldDirectory>,path_bytes:usize,uid:u32,gid:u32,limit:usize}
impl Default for DirectoryCustody {fn default()->Self{Self{held:BTreeMap::new(),path_bytes:0,uid:0,gid:0,limit:MAX_DIRECTORY_PATHS}}}
fn version(path:&Path,m:&fs::Metadata,uid:u32,gid:u32)->Result<Stamp>{
    ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==uid&&m.gid()==gid&&m.mode()&0o022==0,"image directory is not original immutable owner: {}",path.display());
    Ok(Stamp{path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})
}
impl DirectoryCustody {
    pub(super) fn capture(&mut self,path:&Path)->Result<Stamp>{self.capture_inner(path,||Ok(()))}
    fn capture_inner(&mut self,path:&Path,after_open:impl FnOnce()->Result<()>)->Result<Stamp>{
        if let Some(held)=self.held.get(path){self.check_one(path,held)?;return Ok(held.before.as_ref().unwrap().clone())}
        let bytes=path.as_os_str().as_bytes().len();ensure!(path.is_absolute()&&bytes<=MAX_PATH_BYTES&&path.components().all(|c|matches!(c,Component::RootDir|Component::Normal(_))),"image directory path outside fixed bounds");
        ensure!(self.held.len()<self.limit&&self.held.len()<MAX_DIRECTORY_PATHS&&self.path_bytes.checked_add(bytes).is_some_and(|v|v<=MAX_TOTAL_PATH_BYTES),"image directory descriptor/path budget exceeded");
        // Actual slot exists BEFORE metadata/open/final-check failures. A
        // partial owner cannot be replaced by a later successful pathname read.
        self.held.insert(path.into(),HeldDirectory{file:None,before:None,complete:false});self.path_bytes+=bytes;
        let held=self.held.get_mut(path).unwrap();
        held.before=Some(version(path,&fs::symlink_metadata(path)?,self.uid,self.gid)?);
        let mut limit=std::mem::MaybeUninit::<libc::rlimit>::uninit();ensure!(unsafe{libc::getrlimit(libc::RLIMIT_NOFILE,limit.as_mut_ptr())}==0,"image directory RLIMIT unavailable");
        let limit=unsafe{limit.assume_init()};ensure!(limit.rlim_cur==libc::RLIM_INFINITY||(self.held.len()as u64).checked_add(OTHER_FD_RESERVE).is_some_and(|v|v<limit.rlim_cur as u64),"image directory FD reservation exceeds process soft limit");
        let held=self.held.get_mut(path).unwrap();
        held.file=Some(OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?);
        let original=held.before.as_ref().unwrap();ensure!(version(path,&held.file.as_ref().unwrap().metadata()?,self.uid,self.gid)?==*original,"image directory changed during descriptor open: {}",path.display());
        after_open()?;
        ensure!(version(path,&fs::symlink_metadata(path)?,self.uid,self.gid)?==*original,"image directory changed after descriptor open: {}",path.display());
        held.complete=true;Ok(original.clone())
    }
    fn check_one(&self,path:&Path,held:&HeldDirectory)->Result<()> {
        ensure!(held.complete,"original image directory capture incomplete: {}",path.display());
        let before=held.before.as_ref().context("original directory stamp absent")?;
        ensure!(version(path,&held.file.as_ref().context("original directory descriptor absent")?.metadata()?,self.uid,self.gid)?==*before
            &&version(path,&fs::symlink_metadata(path)?,self.uid,self.gid)?==*before,"captured image directory identity/version changed: {}",path.display());Ok(())
    }
    pub(super) fn check_path(&self,path:&Path,before:&Stamp)->Result<()> {
        let held=self.held.get(path).context("original image directory descriptor absent")?;
        ensure!(held.before.as_ref()==Some(before),"image directory initial stamp differs: {}",path.display());self.check_one(path,held)
    }
    pub(super) fn check_all_with(&self,mut check:impl FnMut()->Result<()>)->Result<()> {for(path,held)in &self.held{check()?;self.check_one(path,held)?;}check()}
}

#[cfg(test)]
#[path="image_directory_custody_tests.rs"]
mod tests;
