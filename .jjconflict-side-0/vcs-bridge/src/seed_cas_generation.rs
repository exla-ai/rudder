//! Incremental sanitized seed DATA from an actually held, quiesced integration
//! repository. Native owns the publication/lease lock and accepted authority.
//! No path/cache/manifest constructor here grants project or task permission.
use super::*;
use anyhow::Context;
use rudder_engine::{execution::finalization_budget::FinalizationBudget,
    store::{DirectoryEntry, DirectoryManifest, EntryNode, LocalStore}};
use std::{os::unix::fs::{MetadataExt, OpenOptionsExt}, sync::{Arc, Mutex,
    atomic::{AtomicBool, Ordering}}};

const MAX_SEED_FILES: usize = 1023; // Root's 1024th file is manifest.json.
const MAX_SEED_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SEED_ENTRIES: usize = 4096;
const MAX_SEED_MANIFEST: u64 = 256 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct Version { path: PathBuf, dev:u64, ino:u64, len:u64, mode:u32, uid:u32,
    mtime:(i64,i64), ctime:(i64,i64) }
fn version(path:&Path)->Result<Version> {
    let m=fs::symlink_metadata(path)?;
    ensure!(!m.file_type().is_symlink() && m.uid()==unsafe{libc::geteuid()}
        && m.mode()&0o022==0 && (m.is_file()||m.is_dir()), "seed source is not privately owned regular metadata");
    Ok(Version {path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),uid:m.uid(),
        mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})
}
#[derive(Clone)]
struct CachedFile { version:Version, object:ObjectRef, descriptor:SeedFile }
struct Snapshot { root:ObjectRef, files:BTreeMap<PathBuf,CachedFile> }

/// An original process-local cache of verified file versions and CAS refs.
/// Never serialized, reopened from a ready file, or populated from caller refs.
pub struct RetainedSeedCasCache { pub(super) root:PathBuf, directory:File, id:(u64,u64),
    pub(super) store:LocalStore, snapshot:Mutex<Option<Arc<Snapshot>>>, profile:SeedProfile,
    pub(super) paged_snapshot:Mutex<Option<Arc<super::paged::PagedSnapshot>>> }
impl RetainedSeedCasCache {
    pub fn capture(root:&Path,store:LocalStore)->Result<Arc<Self>> {Self::capture_profile(root,store,SeedProfile::SmallV1)}
    pub fn profile(&self)->SeedProfile {self.profile}
    pub fn capture_profile(root:&Path,store:LocalStore,profile:SeedProfile)->Result<Arc<Self>> {
        ensure!(root.is_absolute() && root.canonicalize()?==root,"seed source root redirects");
        internal_git_directory(root)?;
        let directory=OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(root)?;
        let v=version(root)?;ensure!(v.mode&u32::from(libc::S_IFMT)==u32::from(libc::S_IFDIR),"seed source root is not a directory");
        let held=Arc::new(Self {root:root.into(),directory,id:(v.dev,v.ino),store,snapshot:Mutex::new(None),profile,paged_snapshot:Mutex::new(None)});
        held.check_current()?;Ok(held)
    }
    pub fn check_current(&self)->Result<()> {
        let named=version(&self.root)?;let held=self.directory.metadata()?;
        ensure!((named.dev,named.ino)==self.id && (held.dev(),held.ino())==self.id
            && self.root.canonicalize()?==self.root,"original seed source root replaced");
        internal_git_directory(&self.root)?;Ok(())
    }
}

#[derive(Clone,Copy,Default,Debug,PartialEq,Eq)]
pub struct SeedCasWork { pub read_files:u64, pub read_bytes:u64, pub reused_files:u64 }
/// Stage before CAS effects. Failed/late builds keep partial objects, pin intent,
/// actual cache/root and previous snapshot. Drop never unpins or deletes history.
pub struct SeedCasGeneration {cache:Arc<RetainedSeedCasCache>, previous:Option<Arc<Snapshot>>,
    original:FinalizationBudget,cancelled:Arc<AtomicBool>,baseline:CommitIdentity,operation:JjOperationId,
    stage:Option<String>,files:BTreeMap<PathBuf,CachedFile>,versions:Vec<Version>,
    entries:usize,metadata:BTreeMap<String,SeedFile>,objects:BTreeMap<String,SeedFile>,tree:BTreeMap<String,ObjectRef>,
    root:Option<ObjectRef>,pin:String,pin_attempted:bool,acknowledged:bool,attempted:bool,work:SeedCasWork}
impl SeedCasGeneration {
    pub fn prepare_retaining(out:&mut Option<Self>,cache:Arc<RetainedSeedCasCache>,
        baseline:CommitIdentity,operation:JjOperationId,pin:String,previous_seed:&ObjectRef,
        original:FinalizationBudget,cancelled:Arc<AtomicBool>,check_owner:&mut impl FnMut()->Result<()>)->Result<()> {
        ensure!(out.is_none(),"original seed generation carrier occupied");
        *out=Some(Self {cache,previous:None,original,cancelled,baseline,operation,stage:None,files:BTreeMap::new(),versions:Vec::new(),entries:0,metadata:BTreeMap::new(),objects:BTreeMap::new(),tree:BTreeMap::new(),root:None,pin,pin_attempted:false,acknowledged:false,attempted:false,work:SeedCasWork::default()});
        out.as_mut().unwrap().prepare(previous_seed,check_owner)
    }
    fn time(&self)->Result<()> {if self.cancelled.load(Ordering::Acquire){self.original.cancel();anyhow::bail!("original seed generation cancelled");}self.original.remaining()?;Ok(())}
    fn phase(&self,check_owner:&mut impl FnMut()->Result<()>)->Result<()> {self.time()?;check_owner()?;self.time()}
    fn prepare(&mut self,previous_seed:&ObjectRef,check_owner:&mut impl FnMut()->Result<()>)->Result<()> {
        ensure!(!self.attempted,"seed generation cannot replay");self.attempted=true;self.time()?;
        self.baseline.validate()?;self.operation.validate()?;self.cache.check_current()?;
        let retention=self.cache.store.retention()?;
        ensure!(previous_seed.kind==ObjectKind::Directory && retention.pins.values().any(|r|r==previous_seed)
            && !retention.pins.contains_key(&self.pin),"seed predecessor is not retained or original pin already exists");
        self.previous=self.cache.snapshot.lock().map_err(|_|anyhow::anyhow!("seed version cache poisoned"))?.clone();self.time()?;
        if let Some(previous)=&self.previous {ensure!(retention.pins.values().any(|r|r==&previous.root),"cached original seed lost retention");}
        self.phase(check_owner)?;self.stage=Some(self.cache.store.begin_staging(std::time::Duration::from_secs(120))?.id);
        let git=internal_git_directory(&self.cache.root)?;
        let jj=self.cache.root.join(".jj");self.collect(&jj,Path::new(".jj"),true,true,check_owner)?;
        for part in ["index","refs","packed-refs","shallow"] {
            let path=git.join(part);match fs::symlink_metadata(&path) {
                Ok(_)=>self.collect(&path,&Path::new(".git").join(part),true,false,check_owner)?,
                Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into())}
        }
        self.collect(&git.join("objects"),Path::new(""),false,false,check_owner)?;
        // Git HEAD is seed output DATA derived from the actual accepted commit;
        // the canonical integration Git HEAD is never guessed or mutated.
        let expected_head=format!(".jj/repo/op_heads/heads/{}",self.operation.0);
        let heads=self.metadata.keys().filter(|n|n.starts_with(".jj/repo/op_heads/heads/")).collect::<Vec<_>>();
        ensure!(heads==[&expected_head],"seed operation heads differ from exact accepted operation");
        self.fixed(".git/HEAD",format!("{}\n",self.baseline.commit.0).as_bytes(),check_owner)?;
        self.fixed(".git/config",SAFE_GIT_CONFIG,check_owner)?;
        self.fixed(".jj/repo/store/git_target",b"../../../.git",check_owner)?;
        let manifest=MetadataTemplate {format_version:1,jj_version:"0.40.0".into(),baseline:self.baseline.clone(),operation:self.operation.clone(),metadata:self.metadata.clone(),objects:self.objects.clone()};
        manifest.validate()?;let bytes=canonical_json(&manifest)?;
        ensure!(bytes.len() as u64<=MAX_SEED_MANIFEST,"seed manifest exceeds bounded format");
        let total=self.metadata.values().chain(self.objects.values()).try_fold(bytes.len() as u64,|sum,f|sum.checked_add(f.size_bytes).context("seed bytes overflow"))?;
        ensure!(total<=MAX_SEED_BYTES&&self.metadata.len()+self.objects.len()+1<=MAX_SEED_FILES+1,"complete seed exceeds Root small profile");
        self.phase(check_owner)?;self.tree.insert("manifest.json".into(),self.cache.store.put_blob(&bytes)?);
        let mut entries=0;self.root=Some(directory(&self.cache.store,"",&self.tree,&self.original,&self.cancelled,check_owner,&mut entries)?);self.check_captured()?;
        self.phase(check_owner)?;self.pin_attempted=true;
        let mut anchors=vec![previous_seed.clone()];if let Some(previous)=&self.previous {if !anchors.contains(&previous.root){anchors.push(previous.root.clone());}}
        self.cache.store.publish_extending(self.stage.as_ref().unwrap(),&self.pin,self.root.as_ref().unwrap(),&anchors)?;
        self.check_captured()?;self.phase(check_owner)?;
        let snapshot=Arc::new(Snapshot {root:self.root.as_ref().unwrap().clone(),files:self.files.clone()});
        *self.cache.snapshot.lock().map_err(|_|anyhow::anyhow!("seed version cache poisoned"))?=Some(snapshot);
        self.phase(check_owner)?;self.acknowledged=true;Ok(())
    }
    fn collect(&mut self,path:&Path,relative:&Path,metadata:bool,sanitize:bool,check_owner:&mut impl FnMut()->Result<()>)->Result<()> {
        self.phase(check_owner)?;let name=relative.to_str().context("seed source path is not UTF8")?.to_owned();
        if sanitize && (name==INTERNAL_GIT || name.ends_with("/config.toml") || name.ends_with("/rudder-prototype.log") || name.ends_with("/rudder-state.lock")){return Ok(());}
        if metadata && matches!(name.as_str(),".jj/repo/store/git_target"|".git/HEAD"|".git/config"){return Ok(());}
        let before=version(path)?;
        self.entries+=1;ensure!(self.entries<=MAX_SEED_ENTRIES,"seed source namespace exceeds small profile");
        let output=if metadata{format!("metadata/{name}")}else if name.is_empty(){"objects".into()}else{format!("objects/{name}")};
        small_path(&output)?;
        if before.mode&u32::from(libc::S_IFMT)==u32::from(libc::S_IFDIR) {
            let mut children=fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
            children.sort_by_key(|e|e.file_name());
            ensure!(children.len()+self.files.len()<=MAX_SEED_FILES,"seed namespace exceeds small profile");
            for child in children {self.collect(&child.path(),&relative.join(child.file_name()),metadata,sanitize,check_owner)?;}
        } else {
            ensure!(self.metadata.len()+self.objects.len()<MAX_SEED_FILES && before.len<=MAX_SEED_BYTES,"seed exceeds small profile");
            let cached=self.previous.as_ref().and_then(|p|p.files.get(path)).filter(|p|p.version==before).cloned();
            let cached=if let Some(cached)=cached {self.work.reused_files+=1;cached} else {
                let mut file=atomic::open_read(path)?;let m=file.metadata()?;ensure!((m.dev(),m.ino(),m.len())==(before.dev,before.ino,before.len),"seed file changed while opening");
                // This function also descends directories. Keep the I/O buffer
                // off its recursive stack so the depth guard can reject safely
                // on the standard worker stack.
                let mut bytes=Vec::with_capacity(usize::try_from(before.len)?);let mut chunk=vec![0;64*1024];
                loop {self.time()?;let n=file.read(&mut chunk)?;if n==0{break;}ensure!((bytes.len()+n) as u64<=before.len,"seed file grew during import");bytes.extend_from_slice(&chunk[..n]);}
                ensure!(bytes.len() as u64==before.len && version(path)?==before,"seed file changed during import");
                self.work.read_files+=1;self.work.read_bytes+=before.len;
                self.phase(check_owner)?;CachedFile {version:before.clone(),object:self.cache.store.put_blob(&bytes)?,descriptor:SeedFile{sha256:hex::encode(Sha256::digest(&bytes)),size_bytes:bytes.len() as u64}}
            };
            if metadata {self.metadata.insert(name.clone(),cached.descriptor.clone());self.tree.insert(format!("metadata/{name}"),cached.object.clone());}
            else {self.objects.insert(name.clone(),cached.descriptor.clone());self.tree.insert(format!("objects/{name}"),cached.object.clone());}
            self.files.insert(path.into(),cached);
            let total=self.metadata.values().chain(self.objects.values()).try_fold(0u64,|sum,f|sum.checked_add(f.size_bytes).context("seed bytes overflow"))?;
            ensure!(total<=MAX_SEED_BYTES,"seed bytes exceed Root small profile");
        }
        ensure!(version(path)?==before,"seed namespace changed during import");self.versions.push(before);self.time()
    }
    fn fixed(&mut self,name:&str,bytes:&[u8],check_owner:&mut impl FnMut()->Result<()>)->Result<()> {
        self.phase(check_owner)?;let object=self.cache.store.put_blob(bytes)?;
        self.metadata.insert(name.into(),SeedFile {sha256:hex::encode(Sha256::digest(bytes)),size_bytes:bytes.len() as u64});
        self.tree.insert(format!("metadata/{name}"),object);Ok(())
    }
    pub fn check_captured(&self)->Result<()> {self.time()?;self.cache.check_current()?;
        for prior in &self.versions {ensure!(version(&prior.path)?==*prior,"captured seed source mutated");}
        if self.pin_attempted {ensure!(self.cache.store.retention()?.pins.get(&self.pin)==self.root.as_ref(),"original seed generation pin changed");}self.time()}
    pub fn root(&self)->Result<&ObjectRef> {ensure!(self.acknowledged,"original seed generation ACK absent");self.root.as_ref().context("seed root absent")}
    pub fn work(&self)->SeedCasWork {self.work}
}
fn directory(store:&LocalStore,prefix:&str,files:&BTreeMap<String,ObjectRef>,original:&FinalizationBudget,cancelled:&AtomicBool,check_owner:&mut impl FnMut()->Result<()>,count:&mut usize)->Result<ObjectRef> {
    original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"original seed directory job cancelled");
    let start=if prefix.is_empty(){String::new()}else{format!("{prefix}/")};
    let mut children:BTreeMap<String,Option<ObjectRef>>=BTreeMap::new();
    for (name,object) in files {if let Some(rest)=name.strip_prefix(&start) {let (part,descendant)=rest.split_once('/').map_or((rest,false),|(a,_)|(a,true));
        if descendant {children.entry(part.into()).or_insert(None);}else {ensure!(children.insert(part.into(),Some(object.clone())).is_none(),"duplicate seed path");}}}
    let mut entries=Vec::new();for (name,object)in children {*count+=1;ensure!(*count<=MAX_SEED_ENTRIES,"seed output namespace exceeds small profile");let node=if let Some(object)=object{EntryNode::File{object,mode:0o600}}else{EntryNode::Directory{object:directory(store,&format!("{start}{name}"),files,original,cancelled,check_owner,count)?}};entries.push(DirectoryEntry::new(name.as_bytes(),node)?);}
    original.remaining()?;ensure!(!cancelled.load(Ordering::Acquire),"original seed directory job cancelled");check_owner()?;let root=store.put_directory(DirectoryManifest {format_version:1,mode:0o700,entries})?;ensure!(root.size_bytes<=MAX_SEED_MANIFEST,"seed directory exceeds Root small profile");Ok(root)
}

fn small_path(path:&str)->Result<()> {
    ensure!(!path.is_empty()&&path.len()<=512&&Path::new(path).components().count()<=32
        &&path.split('/').all(|part|!part.is_empty()&&part!="."&&part!="..")
        &&path.bytes().all(|b|b.is_ascii_alphanumeric()||b"._-/".contains(&b)),"seed path exceeds Root small profile");Ok(())
}
