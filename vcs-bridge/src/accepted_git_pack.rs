//! One bounded self-contained pack/index, retained before all effects. No Drop IO.
//! Pinned gix encodes pack/object headers and supplies SHA1, CRC32 and zlib.
use anyhow::{Context,Result,ensure};
use gix::{ObjectId,objs::Kind};
use rudder_engine::execution::finalization_budget::FinalizationBudget;
use std::{fs::{self,File,OpenOptions},io::{Write,Read},path::{Path,PathBuf},os::unix::{fs::{MetadataExt,OpenOptionsExt,PermissionsExt,FileExt,DirBuilderExt},ffi::OsStrExt},sync::{Arc,atomic::{AtomicBool,Ordering}}};
use super::GitImportLimits;
pub(super) const MAX_PACK_BYTES:u64=64<<20;
use gix::features::zlib::{Status,stream::deflate::{Compress,FlushCompress}};
#[derive(Clone)]
struct Entry {id:ObjectId,offset:u64,crc:u32}
struct Directory {path:PathBuf,file:File,id:(u64,u64)}
impl Directory {
 fn open(path:&Path)->Result<Self>{ensure!(path.canonicalize()?==path,"Git pack directory redirects");
  let file=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path)?;
  let m=file.metadata()?;let value=Self{path:path.into(),file,id:(m.dev(),m.ino())};value.current()?;Ok(value)}
 fn current(&self)->Result<()>{for m in [self.file.metadata()?,fs::symlink_metadata(&self.path)?]{ensure!(m.is_dir()&&!m.file_type().is_symlink()&&(m.dev(),m.ino())==self.id&&m.uid()==unsafe{libc::geteuid()}&&m.mode()&0o022==0,"Git pack directory custody changed");}Ok(())}
}
#[derive(Clone,PartialEq,Eq)]
struct Stamp {dev:u64,ino:u64,len:u64,mode:u32,uid:u32,gid:u32,links:u64,mtime:(i64,i64),ctime:(i64,i64)}
fn stamp(file:&File)->Result<Stamp>{let m=file.metadata()?;ensure!(m.is_file()&&m.uid()==unsafe{libc::geteuid()}&&m.nlink()==1&&m.mode()&0o7777==0o444,"Git pack file custody changed");
 Ok(Stamp{dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),uid:m.uid(),gid:m.gid(),links:m.nlink(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})}
/// Non-Serde actual file/directory owners. Failures retain partial bytes and
/// install flags, not an acknowledgment. Never unlink, unkeep or retry on Drop.
pub(super) struct PackBuilder {
 objects:Option<Directory>,directory:Option<Directory>,pack:Option<File>,index:Option<File>,keep:Option<File>,
 pack_temp:PathBuf,index_temp:PathBuf,keep_temp:PathBuf,pack_final:Option<PathBuf>,index_final:Option<PathBuf>,keep_final:Option<PathBuf>,
 pack_stamp:Option<Stamp>,index_stamp:Option<Stamp>,keep_stamp:Option<Stamp>,
 original:FinalizationBudget,cancelled:Arc<AtomicBool>,limits:GitImportLimits,compressor:Option<Compress>,entries:Vec<Entry>,
 seal_attempted:bool,pub(super) stage:&'static str,pub(super) bytes:u64,pub(super) pack_synced:bool,pub(super) index_synced:bool,
 pub(super) keep_synced:bool,pub(super) directory_syncs:u64,pub(super) pack_install_attempted:bool,
 pub(super) index_install_attempted:bool,pub(super) keep_install_attempted:bool,pub(super) pack_installed:bool,
 pub(super) index_installed:bool,pub(super) keep_installed:bool,maximum:u64,
}
impl PackBuilder {
 pub(super) fn capture_retaining(out:&mut Option<Self>,objects:&Path,limits:GitImportLimits,original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Result<()> {
  ensure!(out.is_none(),"original Git pack carrier occupied");let directory=objects.join("pack");let nonce=uuid::Uuid::new_v4();
  let maximum=MAX_PACK_BYTES;
  *out=Some(Self{objects:None,directory:None,pack:None,index:None,keep:None,
   pack_temp:directory.join(format!(".rudder-{nonce}.pack.partial")),index_temp:directory.join(format!(".rudder-{nonce}.idx.partial")),keep_temp:directory.join(format!(".rudder-{nonce}.keep.partial")),
   pack_final:None,index_final:None,keep_final:None,pack_stamp:None,index_stamp:None,keep_stamp:None,
   original,cancelled,limits,compressor:Some(Compress::new()),entries:Vec::new(),seal_attempted:false,stage:"capture",bytes:0,
   pack_synced:false,index_synced:false,keep_synced:false,directory_syncs:0,pack_install_attempted:false,index_install_attempted:false,keep_install_attempted:false,
   pack_installed:false,index_installed:false,keep_installed:false,maximum});
  let owner=out.as_mut().unwrap();owner.tick()?;owner.objects=Some(Directory::open(objects)?);
  match fs::symlink_metadata(&directory){Ok(m)=>ensure!(m.is_dir()&&!m.file_type().is_symlink(),"Git pack directory is not real"),Err(e)if e.kind()==std::io::ErrorKind::NotFound=>fs::DirBuilder::new().mode(0o700).create(&directory)?,Err(e)=>return Err(e.into())}
  owner.directory=Some(Directory::open(&directory)?);owner.pack=Some(create(&owner.pack_temp)?);
  owner.index=Some(create(&owner.index_temp)?);owner.keep=Some(create(&owner.keep_temp)?);
  owner.stage="pack-header";let header=gix::odb::pack::data::header::encode(gix::odb::pack::data::Version::V2,0);owner.write_pack(&header)?;owner.tick()
 }
 fn tick(&self)->Result<()>{if self.cancelled.load(Ordering::Acquire){self.original.cancel();anyhow::bail!("original Git pack import cancelled");}self.original.remaining()?;if let Some(d)=&self.objects{d.current()?;}if let Some(d)=&self.directory{d.current()?;}Ok(())}
 fn write_pack(&mut self,b:&[u8])->Result<()>{self.tick()?;let next=self.bytes.checked_add(b.len()as u64).context("Git pack byte overflow")?;ensure!(next<=self.maximum,"Git pack encoded bytes exceed bounded profile");self.pack.as_mut().context("original Git pack FD absent")?.write_all(b).context("write retained Git pack")?;self.bytes=next;self.tick()}
 pub(super) fn can_fit(&self,length:u64)->Result<bool>{let estimate=length.checked_add(length/8).and_then(|n|n.checked_add(64)).context("Git pack entry bound overflow")?;ensure!(estimate+32<=MAX_PACK_BYTES,"Git object exceeds bounded64MiB pack entry profile");Ok(self.bytes.checked_add(estimate).and_then(|n|n.checked_add(20)).is_some_and(|n|n<=MAX_PACK_BYTES))}
 pub(super) fn add(&mut self,id:ObjectId,kind:Kind,bytes:&[u8])->Result<()> {
  self.tick()?;ensure!((self.entries.len()as u64)<self.limits.max_objects&&bytes.len()as u64<=self.limits.max_object_bytes,"Git pack entry exceeds profile");self.stage="pack-object";
  let offset=self.bytes;let header=match kind{Kind::Commit=>gix::odb::pack::data::entry::Header::Commit,Kind::Tree=>gix::odb::pack::data::entry::Header::Tree,Kind::Blob=>gix::odb::pack::data::entry::Header::Blob,_=>anyhow::bail!("unsupported Git pack kind")};
  let mut encoded=Vec::with_capacity(16);header.write_to(bytes.len()as u64,&mut encoded)?;let mut crc=gix::features::hash::crc32(&encoded);self.write_pack(&encoded)?;
  // One reused bounded compressor, independent complete base entries. Packed
  // source deltas are resolved by the source ODB; no external bases survive.
  let mut compressor=self.compressor.take().context("original Git compressor absent")?;compressor.reset();
  let result=(||->Result<()> {let mut input=bytes;let mut output=[0u8;65536];loop{self.tick()?;
   let n=input.len().min(65536);let flush=if input.len()<=65536{FlushCompress::Finish}else{FlushCompress::None};
   let before_in=compressor.total_in();let before_out=compressor.total_out();let status=compressor.compress(&input[..n],&mut output,flush).context("compress retained Git pack entry")?;
   let consumed=usize::try_from(compressor.total_in()-before_in)?;let produced=usize::try_from(compressor.total_out()-before_out)?;
   ensure!(consumed<=n&&produced<=output.len(),"Git compressor bounds differ");input=&input[consumed..];
   if produced>0{crc=gix::features::hash::crc32_update(crc,&output[..produced]);self.write_pack(&output[..produced])?;}
   if status==Status::StreamEnd{ensure!(input.is_empty(),"Git compression ended before input");break}
   ensure!(consumed>0||produced>0,"Git compressor made no progress");
  }Ok(())})();self.compressor=Some(compressor);result?;
  self.entries.push(Entry{id,offset,crc});self.tick()
 }
 fn sync_directory(&mut self)->Result<()>{self.directory.as_ref().unwrap().file.sync_all().context("fsync retained Git pack directory")?;self.directory_syncs+=1;self.objects.as_ref().unwrap().file.sync_all().context("fsync retained Git objects directory")?;self.directory_syncs+=1;self.tick()}
 /// Checks run before each publication boundary and after actual sync effects.
 /// Index is exposed last, only after all verified bytes and .keep are durable.
 pub(super) fn seal(&mut self)->Result<()> {
  self.tick()?;ensure!(!self.seal_attempted&&!self.index_install_attempted,"original Git pack sealing cannot replay");self.seal_attempted=true;self.stage="finalize-pack";
  let count=u32::try_from(self.entries.len())?;ensure!(count>0,"empty accepted Git pack");let header=gix::odb::pack::data::header::encode(gix::odb::pack::data::Version::V2,count);
  self.pack.as_ref().unwrap().write_all_at(&header,0)?;
  let mut h=gix::hash::hasher(gix::hash::Kind::Sha1);let mut offset=0;let mut buf=[0u8;65536];
  while offset<self.bytes{self.tick()?;let n=self.pack.as_ref().unwrap().read_at(&mut buf[..usize::try_from((self.bytes-offset).min(65536))?],offset)?;ensure!(n>0,"retained Git pack checksum read truncated");h.update(&buf[..n]);offset+=n as u64;}
  let pack_hash=h.try_finalize()?;self.write_pack(pack_hash.as_slice())?;
  self.pack.as_ref().unwrap().set_permissions(fs::Permissions::from_mode(0o444))?;self.pack.as_ref().unwrap().sync_all().context("fsync verified Git pack")?;self.pack_synced=true;self.tick()?;
  self.stage="finalize-index";self.entries.sort_by_key(|e|e.id);ensure!(self.entries.windows(2).all(|w|w[0].id<w[1].id),"duplicate accepted Git pack OID");
  let cap=self.entries.len().checked_mul(36).and_then(|n|n.checked_add(1072)).context("Git index allocation overflow")?;ensure!(cap<=64<<20,"Git pack index memory exceeds profile");let mut idx=Vec::with_capacity(cap);
  idx.extend_from_slice(b"\xfftOc");idx.extend_from_slice(&2u32.to_be_bytes());let mut fan=[0u32;256];for e in &self.entries{fan[usize::from(e.id.as_bytes()[0])]+=1;}let mut sum=0u32;for n in fan{sum=sum.checked_add(n).context("Git index fanout overflow")?;idx.extend_from_slice(&sum.to_be_bytes());}
  for (i,e)in self.entries.iter().enumerate(){if i%1024==0{self.tick()?;}idx.extend_from_slice(e.id.as_bytes());}
  for e in &self.entries{idx.extend_from_slice(&e.crc.to_be_bytes());}
  let mut large=Vec::new();for e in &self.entries{let value=if e.offset>0x7fff_ffff{let n=u32::try_from(large.len())?;ensure!(n<0x8000_0000,"Git large-offset count exceeds format");large.push(e.offset);n|0x8000_0000}else{e.offset as u32};idx.extend_from_slice(&value.to_be_bytes());}
  for n in large{idx.extend_from_slice(&n.to_be_bytes());}idx.extend_from_slice(pack_hash.as_slice());let mut ih=gix::hash::hasher(gix::hash::Kind::Sha1);ih.update(&idx);idx.extend_from_slice(ih.try_finalize()?.as_slice());ensure!(idx.len()<=cap,"Git index encoded size exceeds allocation bound");
  for chunk in idx.chunks(65536){self.tick()?;self.index.as_mut().unwrap().write_all(chunk).context("write retained Git pack index")?;}
  self.index.as_ref().unwrap().set_permissions(fs::Permissions::from_mode(0o444))?;self.index.as_ref().unwrap().sync_all().context("fsync verified Git index")?;self.index_synced=true;self.tick()?;
  self.keep.as_mut().unwrap().write_all(b"Rudder retained immutable accepted Git history\n")?;self.keep.as_ref().unwrap().set_permissions(fs::Permissions::from_mode(0o444))?;self.keep.as_ref().unwrap().sync_all().context("fsync Git pack GC keep")?;self.keep_synced=true;self.sync_directory()?;
  let base=self.directory.as_ref().unwrap().path.join(format!("pack-{pack_hash}"));self.pack_final=Some(base.with_extension("pack"));self.index_final=Some(base.with_extension("idx"));self.keep_final=Some(base.with_extension("keep"));
  self.compressor=None;self.stage="sealed";self.tick()
 }
 pub(super) fn publish(&mut self,guard:&mut impl FnMut()->Result<()>)->Result<()> {
  self.tick()?;guard()?;ensure!(self.pack_synced&&self.index_synced&&self.keep_synced&&!self.keep_install_attempted,"original Git pack not sealed/already published");
  self.stage="publish-keep";self.keep_install_attempted=true;install(&self.keep_temp,self.keep_final.as_ref().unwrap(),&mut self.keep,&self.original,&self.cancelled)?;self.keep_installed=true;self.keep_stamp=Some(stamp(self.keep.as_ref().unwrap())?);self.sync_directory()?;guard()?;
  self.stage="publish-pack";self.pack_install_attempted=true;install(&self.pack_temp,self.pack_final.as_ref().unwrap(),&mut self.pack,&self.original,&self.cancelled)?;self.pack_installed=true;self.pack_stamp=Some(stamp(self.pack.as_ref().unwrap())?);self.sync_directory()?;guard()?;
  self.stage="publish-index";self.tick()?;self.index_install_attempted=true;install(&self.index_temp,self.index_final.as_ref().unwrap(),&mut self.index,&self.original,&self.cancelled)?;self.index_installed=true;self.index_stamp=Some(stamp(self.index.as_ref().unwrap())?);self.sync_directory()?;
  self.stage="installed";self.current()?;guard()?;self.tick()
 }
 pub(super) fn current(&self)->Result<()>{self.tick()?;for (file,path,saved)in[(&self.pack,&self.pack_final,&self.pack_stamp),(&self.index,&self.index_final,&self.index_stamp),(&self.keep,&self.keep_final,&self.keep_stamp)]{
  if let Some(saved)=saved{let f=file.as_ref().context("original installed Git pack FD absent")?;ensure!(stamp(f)?==*saved,"held installed Git pack changed");let named=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK|libc::O_CLOEXEC).open(path.as_ref().unwrap())?;ensure!(stamp(&named)?==*saved,"named installed Git pack changed");}}
  Ok(())}
 pub(super) fn temporary_paths(&self)->[&Path;3]{[&self.pack_temp,&self.index_temp,&self.keep_temp]}
 pub(super) fn paths(&self)->[Option<&Path>;3]{[self.pack_final.as_deref(),self.index_final.as_deref(),self.keep_final.as_deref()]}
}
fn create(path:&Path)->Result<File>{Ok(OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?)}
fn rename_noreplace(from:&Path,to:&Path)->std::io::Result<()> {let from=std::ffi::CString::new(from.as_os_str().as_bytes())?;let to=std::ffi::CString::new(to.as_os_str().as_bytes())?;
 #[cfg(target_os="linux")]let answer=unsafe{libc::syscall(libc::SYS_renameat2,libc::AT_FDCWD,from.as_ptr(),libc::AT_FDCWD,to.as_ptr(),libc::RENAME_NOREPLACE)};
 #[cfg(target_os="macos")]let answer=unsafe{libc::renamex_np(from.as_ptr(),to.as_ptr(),libc::RENAME_EXCL)};
 #[cfg(not(any(target_os="linux",target_os="macos")))]return Err(std::io::Error::other("atomic no-replace Git pack publication unsupported"));
 #[cfg(any(target_os="linux",target_os="macos"))]if answer==0{Ok(())}else{Err(std::io::Error::last_os_error())}}
fn install(from:&Path,to:&Path,held:&mut Option<File>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
 original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"original Git pack installation cancelled");
 match rename_noreplace(from,to){Ok(())=>Ok(()),Err(e)if e.kind()==std::io::ErrorKind::AlreadyExists=>{
  // Legacy explicit idempotent import: compare actual stored bytes, never just
  // filenames/checksums. Existing files are not repaired, relinked or rewritten.
  let actual=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK|libc::O_CLOEXEC).open(to)?;let before=stamp(&actual)?;let expected=held.as_ref().context("original Git pack file absent")?;let want=stamp(expected)?;ensure!(before.len==want.len,"existing Git pack file length differs");
  let mut a=[0;65536];let mut b=[0;65536];let mut offset=0;while offset<want.len{original.remaining()?;if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original Git pack reuse cancelled");}let n=usize::try_from((want.len-offset).min(65536))?;actual.read_exact_at(&mut a[..n],offset)?;expected.read_exact_at(&mut b[..n],offset)?;ensure!(a[..n]==b[..n],"existing Git pack bytes differ; refusing repair");offset+=n as u64;}
  ensure!(stamp(&actual)?==before&&stamp(expected)?==want,"Git pack changed during idempotent comparison");actual.sync_all().context("fsync actual idempotent Git pack file")?;ensure!(stamp(&actual)?==before,"existing Git pack changed after sync");*held=Some(actual);Ok(())},Err(e)=>Err(e).with_context(||format!("publish retained Git pack file {}",to.display()))}
}
#[cfg(test)]
#[path="accepted_git_pack_tests.rs"]
mod tests;
