//! Explicit PackedHistoryV2. Original compressed bytes are copied once; every
//! object is independently reconstructed/hash-verified before index publication.
//! No subprocess, source mutation, fallback, Drop IO or serialized authority.
use super::*;
use std::{fs::{File,OpenOptions},io::{Read,Write},os::unix::{fs::{MetadataExt,OpenOptionsExt,PermissionsExt,FileExt,DirBuilderExt},ffi::OsStrExt},path::PathBuf};
use gix::features::zlib::{Inflate,FlushDecompress,Status};

pub const MAX_ENCODED_BYTES:u64=384<<20;
pub const MAX_PACK_BYTES:u64=MAX_ENCODED_BYTES;
pub const MAX_OBJECT_BYTES:u64=32<<20;
pub const MAX_OBJECTS:usize=100_000;
pub const MAX_EDGES:u64=1_000_000;
pub const MAX_VERIFICATION_WORK:u64=1<<30;
pub const MAX_DELTA_DEPTH:usize=64;
pub const MAX_CACHE_BYTES:usize=32<<20;
/// One base, output and instruction vector plus bounded verified cache. Index,
/// record/edge tables have separate fixed count bounds, never payload vectors.
pub const MAX_DECODE_BUFFERS:u64=128<<20;
const MAX_INDEX_BYTES:usize=1072+MAX_OBJECTS*28;
#[path="accepted_git_canonical.rs"]mod canonical;
pub use canonical::CanonicalProcessData;

#[derive(Clone,Debug,PartialEq,Eq,serde::Serialize)]
pub struct PackedHistoryData {
 pub profile:&'static str,pub original_constraint:rudder_engine::execution::finalization_budget::FinalizationConstraint,pub encoded_bytes:u64,pub decoded_record_bytes:u64,pub reconstructed_bytes:u64,
 pub verification_work_bytes:u64,pub verification_work_limit_bytes:u64,pub object_limit_bytes:u64,
 pub encoded_limit_bytes:u64,pub delta_depth_limit:usize,pub decode_buffer_limit_bytes:u64,
 pub objects:u64,pub pack_sha1:String,pub canonicalizer:Option<CanonicalProcessData>,pub canonical_child_reaped_and_all_pipes_eof:bool,pub installed:bool,
}
#[derive(Clone,Debug,PartialEq,Eq)]
struct Stamp {dev:u64,ino:u64,len:u64,mode:u32,uid:u32,gid:u32,links:u64,mtime:(i64,i64),ctime:(i64,i64)}
fn stamp(f:&File)->Result<Stamp>{let m=f.metadata()?;ensure!(m.is_file()&&m.nlink()>0&&(m.uid()==0||m.uid()==unsafe{libc::geteuid()})&&m.mode()&0o022==0,"packed source file is not protected regular data");
 Ok(Stamp{dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),uid:m.uid(),gid:m.gid(),links:m.nlink(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})}
fn same_except_rename_ctime(a:&Stamp,b:&Stamp)->bool{a.dev==b.dev&&a.ino==b.ino&&a.len==b.len&&a.mode==b.mode&&a.uid==b.uid&&a.gid==b.gid&&a.links==b.links&&a.mtime==b.mtime}
struct HeldFile{path:PathBuf,file:File,version:Stamp}
impl HeldFile{
 fn open(path:PathBuf)->Result<Self>{let file=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK|libc::O_CLOEXEC).open(&path)?;let version=stamp(&file)?;let held=Self{path,file,version};held.current()?;Ok(held)}
 fn current(&self)->Result<()>{ensure!(stamp(&self.file)?==self.version,"original packed source FD changed");let named=OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK|libc::O_CLOEXEC).open(&self.path)?;ensure!(stamp(&named)?==self.version,"original packed source name changed");Ok(())}
}
struct HeldDir{path:PathBuf,file:File,id:(u64,u64)}
impl HeldDir{
 fn open(path:PathBuf)->Result<Self>{ensure!(path.is_absolute()&&path.canonicalize()?==path,"packed history directory redirects");let file=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&path)?;let m=file.metadata()?;let value=Self{path,file,id:(m.dev(),m.ino())};value.current()?;Ok(value)}
 fn current(&self)->Result<()>{for m in[self.file.metadata()?,fs::symlink_metadata(&self.path)?]{ensure!(m.is_dir()&&!m.file_type().is_symlink()&&(m.dev(),m.ino())==self.id&&(m.uid()==0||m.uid()==unsafe{libc::geteuid()})&&m.mode()&0o022==0,"original packed history directory changed");}Ok(())}
}
#[derive(Clone,Copy,Debug)]enum Base{Direct(Kind),Offset(u64),Reference(ObjectId)}
#[derive(Clone,Debug)]struct Record{id:ObjectId,offset:u64,end:u64,crc:u32,data:u64,record_size:u64,base:Base,base_size:u64,result_size:u64,depth:usize,kind:Option<Kind>}
#[derive(Clone)]struct Verified{kind:Kind,size:u64,edges:Vec<(ObjectId,Kind)>}
/// Every original FD/intent/decode buffer stays here across Err/unwind. No
/// carrier reuse after a failure, and no later pathname receipt can revive it.
pub(super) struct PackedHistory {
 original:FinalizationBudget,cancelled:Arc<AtomicBool>,canonical:Option<canonical::CanonicalInput>,source:Option<HeldDir>,source_objects:Option<HeldDir>,
 source_pack:Option<HeldFile>,source_index:Option<HeldFile>,index_bytes:Vec<u8>,records:Vec<Record>,ids:BTreeMap<ObjectId,usize>,offsets:BTreeMap<u64,usize>,
 cache:BTreeMap<usize,(Arc<Vec<u8>>,u64)>,cache_bytes:usize,cache_serial:u64,active_base:Option<Arc<Vec<u8>>>,active_bytes:Vec<u8>,active_result:Vec<u8>,verified:Vec<Option<Verified>>,
 target_objects:Option<HeldDir>,target_pack:Option<HeldDir>,pack:Option<File>,index:Option<File>,keep:Option<File>,
 temporary:[PathBuf;3],installed:[Option<HeldFile>;3],install_attempted:[bool;3],pack_hash:Option<ObjectId>,data:PackedHistoryData,
}
impl PackedHistory{
 pub(super) fn stage(original:FinalizationBudget,cancelled:Arc<AtomicBool>)->Self{let original_constraint=original.constraint();Self{original,cancelled,canonical:None,source:None,source_objects:None,source_pack:None,source_index:None,index_bytes:Vec::new(),records:Vec::new(),ids:BTreeMap::new(),offsets:BTreeMap::new(),cache:BTreeMap::new(),cache_bytes:0,cache_serial:0,active_base:None,active_bytes:Vec::new(),active_result:Vec::new(),verified:Vec::new(),target_objects:None,target_pack:None,pack:None,index:None,keep:None,temporary:Default::default(),installed:[None,None,None],install_attempted:[false;3],pack_hash:None,
 data:PackedHistoryData{profile:"packed_history_v2",original_constraint,encoded_bytes:0,decoded_record_bytes:0,reconstructed_bytes:0,verification_work_bytes:0,verification_work_limit_bytes:MAX_VERIFICATION_WORK,object_limit_bytes:MAX_OBJECT_BYTES,encoded_limit_bytes:MAX_ENCODED_BYTES,delta_depth_limit:MAX_DELTA_DEPTH,decode_buffer_limit_bytes:MAX_DECODE_BUFFERS,objects:0,pack_sha1:String::new(),canonicalizer:None,canonical_child_reaped_and_all_pipes_eof:false,installed:false}}}
 fn tick(&self)->Result<()>{time(&self.original,&self.cancelled)}
 fn current(&self)->Result<()>{self.tick()?;if let Some(c)=&self.canonical{c.current()?;}for d in[&self.source,&self.source_objects,&self.target_objects,&self.target_pack].into_iter().flatten(){d.current()?;}for f in[&self.source_pack,&self.source_index].into_iter().flatten(){f.current()?;}for f in self.installed.iter().flatten(){f.current()?;}self.tick()}
 fn charge(&mut self,n:u64)->Result<()>{self.data.verification_work_bytes=self.data.verification_work_bytes.checked_add(n).context("packed verification work overflow")?;ensure!(self.data.verification_work_bytes<=MAX_VERIFICATION_WORK,"PackedHistoryV2 reconstructed verification work exceeds1GiB");self.tick()}
 pub(super) fn request_child_stop_retaining(&mut self){if let Some(c)=&mut self.canonical{c.request_kill_retaining();}}
 pub(super) fn poll_child_settlement(&mut self)->Result<bool>{if let Some(c)=&mut self.canonical{c.poll_settlement()}else{Ok(true)}}
 pub(super) fn progress_data(&self)->PackedHistoryData{let mut data=self.data.clone();data.canonicalizer=self.canonical.as_ref().map(|c|c.process_data());data}
 pub(super) fn receipt(&self)->Result<PackedHistoryData>{ensure!(self.data.installed,"packed history original publication absent");self.current()?;Ok(self.progress_data())}
 pub(super) fn verify(&mut self,source:&gix::Repository,target:&GitBackend,expected:ObjectId,check:&mut impl FnMut()->Result<()>)->Result<ImportedGitHeadData>{
  self.tick()?;check()?;self.source_objects=Some(HeldDir::open(source.common_dir().join("objects").canonicalize()?)?);
  self.canonical=Some(canonical::CanonicalInput::stage(self.original.clone(),self.cancelled.clone()));
  let (pack,index)=self.canonical.as_mut().unwrap().produce(source,target,expected,check)?;self.data.canonical_child_reaped_and_all_pipes_eof=self.canonical.as_ref().unwrap().settled_data();ensure!(self.data.canonical_child_reaped_and_all_pipes_eof,"canonical original child/pipe settlement absent");
  self.source=Some(HeldDir::open(self.canonical.as_ref().unwrap().directory()?.to_path_buf())?);
  self.source_pack=Some(HeldFile::open(pack)?);self.source_index=Some(HeldFile::open(index)?);for f in[&self.source_pack,&self.source_index].into_iter().flatten(){ensure!(f.version.mode&0o7777==0o444&&f.version.links==1,"canonical producer output must already be original readonly single-link data");}
  let pack_len=self.source_pack.as_ref().unwrap().version.len;let index_len=self.source_index.as_ref().unwrap().version.len;
  ensure!((32..=MAX_PACK_BYTES).contains(&pack_len)&&index_len<=MAX_INDEX_BYTES as u64,"canonical pack/index exceeds fixed profile");
  self.data.encoded_bytes=pack_len.checked_add(index_len).context("packed history encoded overflow")?;ensure!(self.data.encoded_bytes<=MAX_ENCODED_BYTES,"packed history encoded384MiB admission exceeded");
  self.index_bytes.resize(index_len as usize,0);read_chunks(&self.source_index.as_ref().unwrap().file,&mut self.index_bytes,0,&self.original,&self.cancelled)?;
  self.parse_index(pack_len)?;check()?;self.verify_pack_hash()?;self.preflight()?;check()?;
  self.verified.resize(self.records.len(),None);let mut verified_edges=0u64;
  let verify_order=self.offsets.values().copied().collect::<Vec<_>>();for i in verify_order{self.tick()?;check()?;let bytes=self.decode(i)?;let r=self.records[i].clone();let kind=r.kind.context("preflight kind absent")?;
   let edges=edges(kind,&bytes,&self.original,&self.cancelled)?;verified_edges=verified_edges.checked_add(edges.len()as u64).context("packed metadata edge overflow")?;ensure!(verified_edges<=MAX_EDGES,"packed metadata edge aggregate exceeded");self.verified[i]=Some(Verified{kind,size:r.result_size,edges});}
  let mut known=BTreeMap::new();let mut pending=VecDeque::new();known.insert(expected,Kind::Commit);pending.push_back(expected);let mut edge_count=0u64;
  let mut data=ImportedGitHeadData{head:GitCommitId(expected.to_string()),objects:0,commits:0,trees:0,blobs:0,bytes:0};
  while let Some(id)=pending.pop_front(){self.tick()?;let i=*self.ids.get(&id).context("accepted Git closure missing from canonical pack")?;let object=self.verified[i].as_ref().context("packed object not verified")?;
   ensure!(known[&id]==object.kind,"packed accepted object kind differs");match object.kind{Kind::Commit=>data.commits+=1,Kind::Tree=>data.trees+=1,Kind::Blob=>data.blobs+=1,_=>anyhow::bail!("unsupported packed accepted kind")}
   data.objects+=1;data.bytes=data.bytes.checked_add(object.size).context("packed reconstructed byte overflow")?;
   for (child,kind)in &object.edges{edge_count+=1;ensure!(edge_count<=MAX_EDGES,"packed accepted edge limit exceeded");if let Some(old)=known.get(child){ensure!(old==kind,"packed closure conflicting kind")}else{ensure!(known.len()<MAX_OBJECTS,"packed closure object bound");known.insert(*child,*kind);pending.push_back(*child);}}
  }
  ensure!(known.len()==self.records.len(),"canonical pack contains unrelated objects outside the exact accepted history");
  self.data.objects=data.objects;self.data.reconstructed_bytes=data.bytes;self.current()?;check()?;Ok(data)
 }
 fn parse_index(&mut self,pack_len:u64)->Result<()>{let raw=&self.index_bytes;ensure!(raw.len()>=1072&&&raw[..4]==b"\xfftOc"&&be32(raw,4)?==2,"PackedHistoryV2 requires canonical indexv2");
  let count=be32(raw,8+255*4)? as usize;ensure!((1..=MAX_OBJECTS).contains(&count)&&raw.len()==1072+count*28,"packed index count/length exceeds profile or uses large offsets");
  let expected=oid(&raw[raw.len()-20..])?;ensure!(sha1(&raw[..raw.len()-20])?==expected,"packed index raw SHA1 differs");
  let pack_hash=oid(&raw[raw.len()-40..raw.len()-20])?;self.pack_hash=Some(pack_hash);self.data.pack_sha1=pack_hash.to_string();
  let mut fan=[0u32;256];let names=1032;let crc=names+count*20;let offsets=crc+count*4;let mut prior=None;
  for i in 0..count{self.tick()?;let id=oid(&raw[names+i*20..names+(i+1)*20])?;ensure!(prior.is_none_or(|p|p<id),"packed index unordered/duplicate OID");prior=Some(id);fan[usize::from(id.as_bytes()[0])]+=1;
   let offset=u64::from(be32(raw,offsets+i*4)?);ensure!(offset>=12&&offset<pack_len-20&&offset&0x8000_0000==0,"packed index offset outside fixed extent");
   let index=self.records.len();ensure!(self.ids.insert(id,index).is_none()&&self.offsets.insert(offset,index).is_none(),"packed index duplicate OID/offset");self.records.push(Record{id,offset,end:0,crc:be32(raw,crc+i*4)?,data:0,record_size:0,base:Base::Direct(Kind::Blob),base_size:0,result_size:0,depth:0,kind:None});}
  let mut total=0u32;for(i,n)in fan.into_iter().enumerate(){total+=n;ensure!(be32(raw,8+i*4)?==total,"packed index fanout differs from OIDs");}
  let order=self.offsets.iter().map(|(o,i)|(*o,*i)).collect::<Vec<_>>();ensure!(order[0].0==12,"canonical pack has unindexed prefix");
  for (position,(_,i))in order.iter().enumerate(){self.records[*i].end=order.get(position+1).map_or(pack_len-20,|(o,_)|*o);}
  Ok(())
 }
 fn verify_pack_hash(&self)->Result<()>{let file=&self.source_pack.as_ref().unwrap().file;let len=self.source_pack.as_ref().unwrap().version.len;let mut header=[0;12];file.read_exact_at(&mut header,0)?;
  ensure!(&header[..4]==b"PACK"&&be32(&header,4)?==2&&be32(&header,8)? as usize==self.records.len(),"canonical pack header/version/count differs");
  let mut h=gix::hash::hasher(gix::hash::Kind::Sha1);let mut buffer=[0;65536];let mut offset=0;
  while offset<len-20{self.tick()?;let n=usize::try_from((len-20-offset).min(buffer.len()as u64))?;file.read_exact_at(&mut buffer[..n],offset)?;h.update(&buffer[..n]);offset+=n as u64;}
  let actual=h.try_finalize()?;let mut trailer=[0;20];file.read_exact_at(&mut trailer,len-20)?;ensure!(actual==oid(&trailer)?&&Some(actual)==self.pack_hash,"pack/index/trailer SHA1 binding differs");
  ensure!(self.source_pack.as_ref().unwrap().path.file_name().is_some_and(|n|n==std::ffi::OsStr::new(&format!("pack-{actual}.pack"))),"source pack is not named by its canonical original checksum");self.current()
 }
 fn preflight(&mut self)->Result<()>{
  for i in 0..self.records.len(){self.tick()?;let r=self.records[i].clone();let mut header=[0;64];let n=usize::try_from((r.end-r.offset).min(64))?;self.source_pack.as_ref().unwrap().file.read_exact_at(&mut header[..n],r.offset)?;
   let (base,size,used)=record_header(&header[..n],r.offset)?;ensure!(size<=MAX_OBJECT_BYTES,"packed record exceeds32MiB before allocation");let data=r.offset+used as u64;ensure!(data<r.end,"packed record compressed extent absent");
   let mut crc=0;let mut pos=r.offset;let mut buffer=[0;65536];while pos<r.end{self.tick()?;let n=usize::try_from((r.end-pos).min(65536))?;self.source_pack.as_ref().unwrap().file.read_exact_at(&mut buffer[..n],pos)?;crc=gix::features::hash::crc32_update(crc,&buffer[..n]);pos+=n as u64;}
   ensure!(crc==r.crc,"pack/index entry CRC32 differs");let (base_size,result_size)=if matches!(base,Base::Direct(_)){(0,size)}else{let mut prefix=Vec::new();inflate_to(&self.source_pack.as_ref().unwrap().file,data,r.end,32,true,&mut prefix,&self.original,&self.cancelled)?;let(a,n)=varint(&prefix,0)?;let(b,_)=varint(&prefix,n)?;ensure!(a<=MAX_OBJECT_BYTES&&b<=MAX_OBJECT_BYTES,"delta base/result exceeds32MiB before allocation");(a,b)};
   self.data.decoded_record_bytes=self.data.decoded_record_bytes.checked_add(size).context("packed record count overflow")?;ensure!(self.data.decoded_record_bytes<=MAX_VERIFICATION_WORK,"inflated pack records/instructions exceed1GiB verification work");
   let record=&mut self.records[i];record.base=base;record.data=data;record.record_size=size;record.base_size=base_size;record.result_size=result_size;
  }
  // Resolve only metadata graph, no payload allocation. Every referenced base
  // must exist in THIS pair; cycles/long chains refuse before full decoding.
  for i in 0..self.records.len(){let mut chain=Vec::new();let mut at=i;let kind=loop{self.tick()?;ensure!(chain.len()<=MAX_DELTA_DEPTH&&!chain.contains(&at),"packed delta chain cycle/depth exceeds profile");chain.push(at);
    let r=&self.records[at];match r.base{Base::Direct(kind)=>break kind,Base::Offset(o)=>at=*self.offsets.get(&o).context("OFS delta base absent from canonical pair")?,Base::Reference(id)=>at=*self.ids.get(&id).context("REF delta external/thin base refused")?,}};
   let mut parent=None;for &at in chain.iter().rev(){let r=&self.records[at];if let Some(size)=parent{ensure!(r.base_size==size,"delta declared base length differs");}parent=Some(r.result_size);}
   self.records[i].depth=chain.len()-1;self.records[i].kind=Some(kind);
  }Ok(())
 }
 fn cached(&mut self,i:usize)->Option<Arc<Vec<u8>>>{self.cache_serial+=1;let value=self.cache.get_mut(&i)?;value.1=self.cache_serial;Some(value.0.clone())}
 fn insert_cache(&mut self,i:usize,bytes:Arc<Vec<u8>>){if self.cache.contains_key(&i){return}while self.cache_bytes+bytes.capacity()>MAX_CACHE_BYTES{let old=self.cache.iter().min_by_key(|(_,v)|v.1).map(|(i,_)|*i).unwrap();let value=self.cache.remove(&old).unwrap();self.cache_bytes-=value.0.capacity();}self.cache_serial+=1;self.cache_bytes+=bytes.capacity();self.cache.insert(i,(bytes,self.cache_serial));}
 fn decode(&mut self,i:usize)->Result<Arc<Vec<u8>>>{if let Some(value)=self.cached(i){return Ok(value)}let mut chain=Vec::new();let mut at=i;
  loop{self.tick()?;if let Some(value)=self.cached(at){self.active_base=Some(value);break}chain.push(at);ensure!(chain.len()<=MAX_DELTA_DEPTH+1,"preflight delta depth changed");match self.records[at].base{Base::Direct(_)=>{self.active_base=None;break},Base::Offset(o)=>at=self.offsets[&o],Base::Reference(id)=>at=self.ids[&id]}}
  while let Some(at)=chain.pop(){let r=self.records[at].clone();self.charge(r.record_size)?;inflate_to(&self.source_pack.as_ref().unwrap().file,r.data,r.end,usize::try_from(r.record_size)?,false,&mut self.active_bytes,&self.original,&self.cancelled)?;
   let bytes=match r.base{Base::Direct(_)=>Arc::new(std::mem::take(&mut self.active_bytes)),_=>{self.charge(r.result_size)?;let base=self.active_base.as_ref().context("verified packed delta base absent")?;ensure!(base.len()as u64==r.base_size,"actual delta base length differs");
     apply_delta(base,&self.active_bytes,usize::try_from(r.result_size)?,&mut self.active_result,&self.original,&self.cancelled)?;Arc::new(std::mem::take(&mut self.active_result))}};
   ensure!(bytes.len()as u64==r.result_size&&gix::objs::compute_hash(gix::hash::Kind::Sha1,r.kind.context("packed kind unavailable")?,&bytes)?==r.id,"packed reconstructed object raw SHA1 differs");
   self.insert_cache(at,bytes.clone());self.active_base=Some(bytes);
  }self.active_base.as_ref().cloned().context("packed decoded object absent")
 }
 pub(super) fn install(&mut self,target:&GitBackend,check:&mut impl FnMut()->Result<()>)->Result<()>{self.current()?;check()?;
  self.target_objects=Some(HeldDir::open(target.git_repo_path().join("objects"))?);let directory=self.target_objects.as_ref().unwrap().path.join("pack");let nonce=uuid::Uuid::new_v4();
  self.temporary=[directory.join(format!(".rudder-packed-{nonce}.keep.partial")),self.source_pack.as_ref().unwrap().path.clone(),self.source_index.as_ref().unwrap().path.clone()];
  match fs::symlink_metadata(&directory){Ok(m)=>ensure!(m.is_dir()&&!m.file_type().is_symlink(),"target pack directory redirects"),Err(e)if e.kind()==io::ErrorKind::NotFound=>fs::DirBuilder::new().mode(0o700).create(&directory)?,Err(e)=>return Err(e.into())}
  self.target_pack=Some(HeldDir::open(directory.clone())?);
  self.keep=Some(create(&self.temporary[0])?);self.pack=Some(self.source_pack.as_ref().unwrap().file.try_clone()?);self.index=Some(self.source_index.as_ref().unwrap().file.try_clone()?);
  self.keep.as_mut().unwrap().write_all(b"Rudder original PackedHistoryV2 immutable accepted history\n")?;
  self.keep.as_ref().unwrap().set_permissions(fs::Permissions::from_mode(0o444))?;
  for file in [&self.keep,&self.pack,&self.index].into_iter().flatten(){self.tick()?;file.sync_all()?;self.tick()?;}self.current()?;self.sync()?;
  let hash=self.pack_hash.context("verified original pack hash absent")?;
  for(i,suffix)in["keep","pack","idx"].into_iter().enumerate(){self.current()?;check()?;let path=directory.join(format!("pack-{hash}.{suffix}"));self.install_attempted[i]=true;
   super::pack::rename_noreplace(&self.temporary[i],&path)?;let original=match i{0=>self.keep.as_ref(),1=>self.pack.as_ref(),_=>self.index.as_ref()}.unwrap().try_clone()?;
   let version=stamp(&original)?;if i==1{let source=self.source_pack.as_mut().unwrap();ensure!(same_except_rename_ctime(&source.version,&version),"own pack rename changed file content/identity fields");source.path=path.clone();source.version=version.clone();}if i==2{let source=self.source_index.as_mut().unwrap();ensure!(same_except_rename_ctime(&source.version,&version),"own pack rename changed file content/identity fields");source.path=path.clone();source.version=version.clone();}ensure!(version.mode&0o7777==0o444&&version.links==1,"installed packed history immutable inode differs");self.installed[i]=Some(HeldFile{path,file:original,version});self.sync()?;self.current()?;if i==1{self.verify_pack_hash()?;}check()?;}
  self.data.installed=true;self.current()?;check()?;Ok(())
 }
 fn sync(&self)->Result<()>{self.source.as_ref().unwrap().file.sync_all()?;self.target_pack.as_ref().unwrap().file.sync_all()?;self.target_objects.as_ref().unwrap().file.sync_all()?;self.tick()}
}
fn time(original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original packed history cancelled");}original.remaining()?;Ok(())}
fn be32(bytes:&[u8],offset:usize)->Result<u32>{Ok(u32::from_be_bytes(bytes.get(offset..offset+4).context("packed integer truncated")?.try_into()?))}
fn oid(raw:&[u8])->Result<ObjectId>{ensure!(raw.len()==20,"packed SHA1 length differs");ObjectId::from_hex(hex::encode(raw).as_bytes()).map_err(Into::into)}
fn sha1(bytes:&[u8])->Result<ObjectId>{let mut h=gix::hash::hasher(gix::hash::Kind::Sha1);h.update(bytes);Ok(h.try_finalize()?)}
fn read_chunks(file:&File,out:&mut[u8],offset:u64,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{for(i,b)in out.chunks_mut(65536).enumerate(){time(original,cancelled)?;file.read_exact_at(b,offset+(i*65536)as u64)?;}time(original,cancelled)}
fn varint(bytes:&[u8],mut at:usize)->Result<(u64,usize)>{let mut value=0u64;let mut shift=0;loop{let b=*bytes.get(at).context("delta variable integer truncated")?;at+=1;ensure!(shift<64&&u64::from(b&127)<=u64::MAX>>shift,"delta variable integer overflow");value|=u64::from(b&127)<<shift;if b&128==0{return Ok((value,at))}shift+=7;}}
fn record_header(bytes:&[u8],offset:u64)->Result<(Base,u64,usize)>{let first=*bytes.first().context("pack record header absent")?;let kind=(first>>4)&7;let mut size=u64::from(first&15);let mut at=1;let mut shift=4;let mut b=first;
 while b&128!=0{b=*bytes.get(at).context("pack size header truncated")?;at+=1;ensure!(shift<64&&u64::from(b&127)<=u64::MAX>>shift,"pack size overflow");size|=u64::from(b&127)<<shift;shift+=7;}
 let base=match kind{1=>Base::Direct(Kind::Commit),2=>Base::Direct(Kind::Tree),3=>Base::Direct(Kind::Blob),6=>{let mut b=*bytes.get(at).context("OFS header truncated")?;at+=1;let mut distance=u64::from(b&127);
  while b&128!=0{b=*bytes.get(at).context("OFS header truncated")?;at+=1;distance=distance.checked_add(1).and_then(|n|n.checked_mul(128)).and_then(|n|n.checked_add(u64::from(b&127))).context("OFS distance overflow")?;}
  ensure!(distance>0&&distance<=offset,"OFS delta distance invalid");Base::Offset(offset-distance)},7=>{let id=oid(bytes.get(at..at+20).context("REF header truncated")?)?;at+=20;Base::Reference(id)},_=>anyhow::bail!("unsupported canonical packed object kind")};Ok((base,size,at))}
/// Size is fixed by preflight BEFORE resize. A one-byte excess sink verifies
/// exact stream end/extent without permitting hidden zlib tail or extra output.
fn inflate_to(file:&File,start:u64,end:u64,size:usize,prefix:bool,out:&mut Vec<u8>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{ensure!(size<=MAX_OBJECT_BYTES as usize&&start<end,"packed inflate admission differs");out.clear();if out.capacity()<size{out.try_reserve_exact(size)?;}ensure!(out.capacity()<=MAX_OBJECT_BYTES as usize,"inflate allocation capacity exceeds profile");out.resize(size,0);let mut inflate=Inflate::default();let mut pos=start;let mut written=0;let mut buffer=[0;65536];let mut overflow=[0u8;1];
 loop{time(original,cancelled)?;let n=usize::try_from((end-pos).min(buffer.len()as u64))?;if n>0{file.read_exact_at(&mut buffer[..n],pos)?;}let mut input=&buffer[..n];
  loop{time(original,cancelled)?;let before_in=inflate.state.total_in();let before_out=inflate.state.total_out();let output=if written<size{&mut out[written..]}else{&mut overflow[..]};let status=inflate.state.decompress(input,output,FlushDecompress::None)?;
   let consumed=usize::try_from(inflate.state.total_in()-before_in)?;let produced=usize::try_from(inflate.state.total_out()-before_out)?;ensure!(consumed<=input.len()&&produced<=output.len(),"inflate accounting differs");
   ensure!(written<size||produced==0,"pack record produced undeclared bytes");written+=produced;pos+=consumed as u64;input=&input[consumed..];
   if prefix&&written==size{return Ok(())}if status==Status::StreamEnd{if prefix{out.truncate(written);return Ok(())}ensure!(written==size&&pos==end,"packed zlib length/end differs");return Ok(())}
   ensure!(consumed>0||produced>0,"packed zlib truncated/no progress");if input.is_empty(){break}
  }
 }
}
fn apply_delta(base:&[u8],instructions:&[u8],size:usize,out:&mut Vec<u8>,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()>{let(base_size,mut at)=varint(instructions,0)?;let(result_size,next)=varint(instructions,at)?;at=next;ensure!(base_size==base.len()as u64&&result_size==size as u64&&size<=MAX_OBJECT_BYTES as usize,"delta application preflight differs");out.clear();if out.capacity()<size{out.try_reserve_exact(size)?;}ensure!(out.capacity()<=MAX_OBJECT_BYTES as usize,"delta output capacity exceeds profile");out.resize(size,0);let mut written=0usize;
 while at<instructions.len(){time(original,cancelled)?;let opcode=instructions[at];at+=1;if opcode&128!=0{let mut offset=0usize;let mut length=0usize;for bit in 0..4{if opcode&(1<<bit)!=0{offset|=usize::from(*instructions.get(at).context("delta copy offset truncated")?)<<(bit*8);at+=1;}}
   for bit in 0..3{if opcode&(1<<(bit+4))!=0{length|=usize::from(*instructions.get(at).context("delta copy length truncated")?)<<(bit*8);at+=1;}}if length==0{length=65536;}let end=offset.checked_add(length).context("delta base extent overflow")?;let next=written.checked_add(length).context("delta result extent overflow")?;ensure!(end<=base.len()&&next<=out.len(),"delta copy escapes declared extent");
   for(dst,src)in out[written..next].chunks_mut(65536).zip(base[offset..end].chunks(65536)){time(original,cancelled)?;dst.copy_from_slice(src);}written=next;
  }else{let length=usize::from(opcode);ensure!(length>0,"delta zero opcode invalid");let end=at.checked_add(length).context("delta literal overflow")?;let next=written.checked_add(length).context("delta output overflow")?;ensure!(end<=instructions.len()&&next<=out.len(),"delta literal escapes extent");out[written..next].copy_from_slice(&instructions[at..end]);at=end;written=next;}}
 ensure!(written==size,"delta result incomplete");time(original,cancelled)
}
fn edges(kind:Kind,bytes:&[u8],original:&FinalizationBudget,cancelled:&AtomicBool)->Result<Vec<(ObjectId,Kind)>>{let mut edges=Vec::new();match kind{Kind::Commit=>{let commit=gix::objs::CommitRef::from_bytes(bytes).context("malformed packed Git commit")?;commit.author()?;commit.committer()?;ensure!(commit.parents.len()<=1024,"packed Git commit parent limit");edges.push((commit.tree(),Kind::Tree));for parent in commit.parents(){time(original,cancelled)?;edges.push((parent,Kind::Commit));}},Kind::Tree=>{let mut names=BTreeSet::new();let mut prior=None;for entry in gix::objs::TreeRefIter::from_bytes(bytes){time(original,cancelled)?;let entry=entry?;let name:&[u8]=entry.filename.as_ref();ensure!(!name.is_empty()&&name!=b"."&&name!=b".."&&!name.contains(&b'/')&&names.insert(name)&&prior.is_none_or(|p|p<entry),"malformed packed tree namespace/order");prior=Some(entry);
  use gix::objs::tree::EntryKind;let child=match entry.mode.kind(){EntryKind::Tree=>Kind::Tree,EntryKind::Blob|EntryKind::BlobExecutable|EntryKind::Link=>Kind::Blob,EntryKind::Commit=>anyhow::bail!("Gitlinks unsupported by packed source import")};ensure!(edges.len()<MAX_EDGES as usize,"packed tree edge bound");edges.push((entry.oid.to_owned(),child));}},Kind::Blob=>{},_=>anyhow::bail!("unsupported packed closure kind")};Ok(edges)}
fn create(path:&Path)->Result<File>{Ok(OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?)}


#[cfg(test)]#[path="accepted_git_packed_tests.rs"]mod tests;
