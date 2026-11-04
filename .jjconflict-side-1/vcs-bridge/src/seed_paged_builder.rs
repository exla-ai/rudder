//! Retained V2 initial/successor builder over the actual quiesced repository.
use super::*;
#[derive(Clone,PartialEq,Eq)]
struct Version{path:PathBuf,dev:u64,ino:u64,len:u64,mode:u32,mtime:(i64,i64),ctime:(i64,i64)}
fn version(p:&Path)->Result<Version>{let m=fs::symlink_metadata(p)?;ensure!(!m.file_type().is_symlink()&&(m.is_dir()||m.is_file())
    &&m.uid()==unsafe{libc::geteuid()}&&m.mode()&0o022==0,"seed source custody changed");
    Ok(Version{path:p.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})}
#[derive(Clone)]
struct Cached{version:Version,record:PagedSeedFile}
pub(crate) struct PagedSnapshot{root:ObjectRef,files:BTreeMap<PathBuf,Cached>}
#[derive(Clone,Copy,Default,Debug,PartialEq,Eq)]
pub struct PagedSeedWork{pub read_files:u64,pub read_bytes:u64,pub reused_files:u64}
/// Original mutable carrier, staged before any CAS pin/publication effects.
/// Unknown ACK never refreshes the cache or turns root() into readiness.
pub struct PagedSeedGeneration{
    cache:Arc<RetainedSeedCasCache>,previous:Option<Arc<PagedSnapshot>>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,
    baseline:CommitIdentity,operation:JjOperationId,pin:String,stage:Option<String>,files:BTreeMap<PathBuf,Cached>,versions:Vec<Version>,
    records:BTreeMap<String,PagedSeedFile>,root:Option<ObjectRef>,pin_attempted:bool,ack:bool,attempted:bool,work:PagedSeedWork,metadata_bytes:u64,object_bytes:u64,metadata_files:usize,object_files:usize,
}
impl PagedSeedGeneration{
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_retaining(out:&mut Option<Self>,cache:Arc<RetainedSeedCasCache>,baseline:CommitIdentity,operation:JjOperationId,
        pin:String,previous:Option<&ObjectRef>,original:FinalizationBudget,cancelled:Arc<AtomicBool>,check:&mut impl FnMut()->Result<()>)->Result<()>{
        ensure!(out.is_none()&&cache.profile()==SeedProfile::PagedCowV2,"paged seed original carrier/profile differs");
        *out=Some(Self{cache,previous:None,original,cancelled,baseline,operation,pin,stage:None,files:BTreeMap::new(),versions:Vec::new(),
            records:BTreeMap::new(),root:None,pin_attempted:false,ack:false,attempted:false,work:PagedSeedWork::default(),metadata_bytes:0,object_bytes:0,metadata_files:0,object_files:0});
        out.as_mut().unwrap().prepare(previous,check)
    }
    fn time(&self)->Result<()>{if self.cancelled.load(Ordering::Acquire){self.original.cancel();anyhow::bail!("paged seed original cancelled");}self.original.remaining()?;Ok(())}
    fn phase(&self,check:&mut impl FnMut()->Result<()>)->Result<()>{self.time()?;check()?;self.time()}
    fn prepare(&mut self,previous:Option<&ObjectRef>,check:&mut impl FnMut()->Result<()>)->Result<()>{
        ensure!(!self.attempted,"paged seed cannot replay");self.attempted=true;self.phase(check)?;self.cache.check_current()?;
        self.baseline.validate()?;self.operation.validate()?;let retained=self.cache.store.retention()?;
        ensure!(!retained.pins.contains_key(&self.pin),"paged original pin collision");
        if let Some(root)=previous{ensure!(retained.pins.values().any(|r|r==root),"paged predecessor retention absent");}
        self.previous=self.cache.paged_snapshot.lock().map_err(|_|anyhow::anyhow!("paged cache poisoned"))?.clone();
        if let Some(p)=&self.previous{ensure!(retained.pins.values().any(|r|r==&p.root),"paged cache lost original pin");}
        self.phase(check)?;self.stage=Some(self.cache.store.begin_staging(std::time::Duration::from_secs(120))?.id);
        let git=internal_git_directory(&self.cache.root)?;
        self.collect(&self.cache.root.join(".jj"),Path::new(".jj"),true,true,check)?;
        for name in ["index","refs","packed-refs","shallow"]{let p=git.join(name);match fs::symlink_metadata(&p){Ok(_)=>self.collect(&p,&Path::new(".git").join(name),true,false,check)?,
            Err(e)if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}}
        self.collect(&git.join("objects"),Path::new(""),false,false,check)?;
        self.fixed(".git/HEAD",format!("{}\n",self.baseline.commit.0).as_bytes())?;
        self.fixed(".git/config",SAFE_GIT_CONFIG)?;self.fixed(".jj/repo/store/git_target",b"../../../.git")?;
        let heads=self.records.keys().filter(|p|p.starts_with("metadata/.jj/repo/op_heads/heads/")).collect::<Vec<_>>();
        ensure!(heads==[&format!("metadata/.jj/repo/op_heads/heads/{}",self.operation.0)],"paged accepted operation differs");
        let stage=self.stage.as_ref().unwrap();let adapter=LocalDirectoryStore::new(&self.cache.store,stage);let dirs=PagedDirectory::new(&adapter);
        let metadata=build_tree(&dirs,&self.records,"metadata",self,check)?;let objects=build_tree(&dirs,&self.records,"objects",self,check)?;
        let private=private_root(&self.cache.store,&metadata,&objects,Some(stage))?;
        let values=self.records.values().cloned().collect::<Vec<_>>();let mut pages=Vec::new();
        for (i,files)in values.chunks(V2_RECORDS_PER_PAGE).enumerate(){self.phase(check)?;let bytes=canonical_json(&RecordsPage{format_version:2,files:files.to_vec()})?;
            ensure!(bytes.len()<=V2_MAX_PAGE_BYTES,"paged seed records exceed page bound");let object=put(&self.cache.store,stage,&bytes)?;
            pages.push(DirectoryEntry::new(format!("{i:08}").as_bytes(),EntryNode::File{logical_size:object.size_bytes,object,mode:0o600})?);}
        let records=dirs.build(0o700,pages)?;let mut d=PagedSeedDescriptor{format_version:2,jj_version:"0.40.0".into(),baseline:self.baseline.clone(),operation:self.operation.clone(),
            metadata,objects,private,records,metadata_files:0,object_files:0,metadata_bytes:0,object_bytes:0};
        for f in self.records.values(){if f.path.starts_with("metadata/"){d.metadata_files+=1;d.metadata_bytes+=f.size_bytes}else{d.object_files+=1;d.object_bytes+=f.size_bytes}}
        d.validate()?;let bytes=canonical_json(&d)?;ensure!(bytes.len()<=V2_MAX_DESCRIPTOR_BYTES,"paged descriptor exceeds bound");let descriptor=put(&self.cache.store,stage,&bytes)?;
        let mut roots=vec![DirectoryEntry::new(b"descriptor.json",EntryNode::File{logical_size:descriptor.size_bytes,object:descriptor,mode:0o600})?];
        for(name,object)in[("metadata",d.metadata),("objects",d.objects),("private",d.private),("records",d.records)]{roots.push(DirectoryEntry::new(name.as_bytes(),EntryNode::Directory{object})?);}
        self.root=Some(dirs.build(0o700,roots)?);self.check_captured()?;self.phase(check)?;self.pin_attempted=true;
        let mut anchors=Vec::new();if let Some(r)=previous{anchors.push(r.clone())}if let Some(p)=&self.previous{if !anchors.contains(&p.root){anchors.push(p.root.clone())}}
        if anchors.is_empty(){self.cache.store.publish(stage,&self.pin,self.root.as_ref().unwrap())?;}
        else{self.cache.store.publish_extending(stage,&self.pin,self.root.as_ref().unwrap(),&anchors)?;}
        self.phase(check)?;self.check_captured()?;
        *self.cache.paged_snapshot.lock().map_err(|_|anyhow::anyhow!("paged cache poisoned"))?=Some(Arc::new(PagedSnapshot{root:self.root.as_ref().unwrap().clone(),files:self.files.clone()}));
        self.phase(check)?;self.ack=true;Ok(())
    }
    fn collect(&mut self,p:&Path,relative:&Path,metadata:bool,sanitize:bool,check:&mut impl FnMut()->Result<()>)->Result<()>{
        self.phase(check)?;let name=relative.to_str().context("seed path is not UTF8")?;
        if sanitize&&(name==".jj/repo/store/git"||name.ends_with("/config.toml")||name.ends_with("/rudder-prototype.log")||name.ends_with("/rudder-state.lock")){return Ok(())}
        if metadata&&matches!(name,".jj/repo/store/git_target"|".git/HEAD"|".git/config"){return Ok(())}
        let before=version(p)?;let output=if metadata{format!("metadata/{name}")}else if name.is_empty(){"objects".into()}else{format!("objects/{name}")};path(&output)?;
        ensure!(self.versions.len()<=V2_MAX_FILES*3,"paged source namespace exceeded");
        if before.mode&u32::from(libc::S_IFMT)==u32::from(libc::S_IFDIR){let mut children=fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
            ensure!(children.len()<=V2_MAX_FILES,"paged source directory exceeded");children.sort_by_key(|e|e.file_name());
            for e in children{self.collect(&e.path(),&relative.join(e.file_name()),metadata,sanitize,check)?}}
        else{ensure!(self.records.len()<V2_MAX_FILES&&before.len<=V2_MAX_FILE_BYTES,"paged seed file bound exceeded");
            let cached=self.previous.as_ref().and_then(|s|s.files.get(p)).filter(|c|c.version==before).cloned();
            let cached=if let Some(c)=cached{self.work.reused_files+=1;c}else{
                let file=atomic::open_read(p)?;let m=file.metadata()?;ensure!((m.dev(),m.ino(),m.len())==(before.dev,before.ino,before.len),"seed changed while opening");
                let mut reader=CheckedReader{file,job:self,check,hash:Sha256::new(),bytes:0,maximum:before.len};
                let object=content::import_file(&StagedContentStore::new(&self.cache.store,self.stage.as_ref().unwrap())?,&mut reader)?;
                let read_bytes=reader.bytes;let digest=hex::encode(reader.hash.finalize());ensure!(read_bytes==before.len&&version(p)?==before,"paged seed changed during import");
                self.work.read_files+=1;self.work.read_bytes+=read_bytes;Cached{version:before.clone(),record:PagedSeedFile{path:output.clone(),object,sha256:digest,size_bytes:read_bytes}}};
            let mut record=cached.record.clone();record.path=output;file_record(&record)?;self.records.insert(record.path.clone(),record);self.files.insert(p.into(),cached);
            if metadata{self.metadata_files+=1;self.metadata_bytes=self.metadata_bytes.checked_add(before.len).context("seed metadata overflow")?;}
            else{self.object_files+=1;self.object_bytes=self.object_bytes.checked_add(before.len).context("seed objects overflow")?;}
            ensure!(self.metadata_files<=V2_MAX_METADATA_FILES&&self.object_files<=V2_MAX_OBJECT_FILES
                &&self.metadata_bytes<=V2_MAX_METADATA_BYTES&&self.object_bytes<=V2_MAX_OBJECT_BYTES,"paged seed logical profile exceeded");
        }ensure!(version(p)?==before,"seed namespace changed during import");self.versions.push(before);self.phase(check)
    }
    fn fixed(&mut self,name:&str,bytes:&[u8])->Result<()>{self.time()?;let object=put(&self.cache.store,self.stage.as_ref().unwrap(),bytes)?;
        let record=PagedSeedFile{path:format!("metadata/{name}"),sha256:object.digest.clone(),size_bytes:object.size_bytes,object};self.records.insert(record.path.clone(),record);Ok(())}
    pub fn check_captured(&self)->Result<()>{self.time()?;self.cache.check_current()?;for v in &self.versions{ensure!(version(&v.path)?==*v,"captured seed source changed");}
        if self.pin_attempted{ensure!(self.cache.store.retention()?.pins.get(&self.pin)==self.root.as_ref(),"original seed pin differs");}self.time()}
    pub fn root(&self)->Result<&ObjectRef>{ensure!(self.ack,"paged seed original publication ACK absent");self.root.as_ref().context("paged root absent")}
    pub fn work(&self)->PagedSeedWork{self.work}
}
fn put(s:&LocalStore,stage:&str,bytes:&[u8])->Result<ObjectRef>{let r=ObjectRef::from_bytes(ObjectKind::Blob,bytes);s.receive_staged(stage,&r,bytes)?;Ok(r)}
struct CheckedReader<'a,F:FnMut()->Result<()>>{file:File,job:&'a PagedSeedGeneration,check:&'a mut F,hash:Sha256,bytes:u64,maximum:u64}
impl<F:FnMut()->Result<()>> Read for CheckedReader<'_,F>{fn read(&mut self,b:&mut[u8])->std::io::Result<usize>{
    self.job.phase(self.check).map_err(std::io::Error::other)?;let max=b.len().min(64*1024);let n=self.file.read(&mut b[..max])?;
    self.bytes=self.bytes.checked_add(n as u64).ok_or_else(||std::io::Error::other("seed read overflow"))?;
    if self.bytes>self.maximum{return Err(std::io::Error::other("seed grew during read"))}self.hash.update(&b[..n]);self.job.phase(self.check).map_err(std::io::Error::other)?;Ok(n)}}
fn build_tree<S:directory::DirectoryStore>(dirs:&PagedDirectory<'_,S>,records:&BTreeMap<String,PagedSeedFile>,prefix:&str,
    job:&PagedSeedGeneration,check:&mut impl FnMut()->Result<()>)->Result<ObjectRef>{
    job.phase(check)?;let start=format!("{prefix}/");let mut children:BTreeMap<String,Option<&PagedSeedFile>>=BTreeMap::new();
    for(k,f)in records.range(start.clone()..){let Some(rest)=k.strip_prefix(&start)else{break};let(part,descendant)=rest.split_once('/').map_or((rest,false),|(a,_)|(a,true));
        if descendant{children.entry(part.into()).or_insert(None);}else{ensure!(children.insert(part.into(),Some(f)).is_none(),"seed file/directory collision");}}
    let mut entries=Vec::new();for(name,file)in children{job.phase(check)?;let node=if let Some(f)=file{EntryNode::File{object:f.object.clone(),logical_size:f.size_bytes,mode:0o600}}
        else{EntryNode::Directory{object:build_tree(dirs,records,&format!("{start}{name}"),job,check)?}};entries.push(DirectoryEntry::new(name.as_bytes(),node)?);}
    let root=dirs.build(0o700,entries)?;job.phase(check)?;Ok(root)
}
