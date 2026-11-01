//! Deterministic fixed-tree image DATA builder. No Root/engine/project/task
//! capability is constructed here. Both the source-free build helper and actual
//! retained runtime importer call this same code, then apply their own custody.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use crate::engine_registration::ImagePreseedBudget;
use rudder_engine::execution::provider_kernel::{PROVIDER_KERNEL_DIRECTORY,PROVIDER_KERNEL_EMPTY_MODE,PROVIDER_SWAP_TOOL,PROVIDER_SWAP_EMPTY_MODE,is_provider_kernel_path};
use rudder_engine::{content::{self,ContentStore,StagedContentStore},directory::{self,DirectoryStore,EntryNode,DirectoryEntry,LocalDirectoryStore,PagedDirectory},
    protocol::{canonical_json,ObjectKind,ObjectRef},store::{LocalStore,LocalImageImport,LocalImagePinAck,ImageImportFence,ImageImportStats}};
use serde::{Serialize,Deserialize};
use sha2::{Digest,Sha256};
use std::{collections::{BTreeSet,BTreeMap},fs::{self,File,OpenOptions},io::Read,os::unix::fs::{FileExt,MetadataExt,OpenOptionsExt},path::{Path,PathBuf},sync::{Arc,atomic::{AtomicBool,Ordering}},time::Duration};
#[path="image_directory_custody.rs"]
mod directory_custody;
use directory_custody::DirectoryCustody;
use crate::compiler_image_data::{CompilerSource,CompilerArtifactBindingData};
const MAX_ENTRIES:usize=65536;
const MAX_BYTES:u64=2*1024*1024*1024;
const MAX_DEPTH:usize=128;
const MAX_MANIFEST:usize=32*1024*1024;
const MODEL:&str="/opt/rudder-managed-runtime/v1";
const PACKAGE:&str="/opt/rudder";
const WHOLE_DIRS:[(&str,&str);4]=[("usr","/usr"),("model",MODEL),("package",PACKAGE),("certificates","/etc/ssl/certs")];
const STATIC_DIRS:[(&str,&str);1]=[("alternatives","/etc/alternatives")];
const STATIC_FILES:[(&str,&str);4]=[("loader-cache","/etc/ld.so.cache"),("timezone","/etc/localtime"),("ssl-config","/etc/ssl/openssl.cnf"),("python-site","/etc/python3.11/sitecustomize.py")];
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageFileData {pub path:PathBuf,pub sha256:String,pub size_bytes:u64,pub content:ObjectRef}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct ImageManifestData {pub format_version:u32,pub installer_input:ObjectRef,pub files:Vec<ImageFileData>}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct ImagePreseedPinData {pub format_version:u32,pub manifest:ObjectRef,pub installer_input:ObjectRef}
#[derive(Clone,PartialEq,Eq)]
struct Stamp {path:PathBuf,dev:u64,ino:u64,len:u64,mode:u32,mtime:(i64,i64),ctime:(i64,i64)}
fn same_inode_version(a:&Stamp,b:&Stamp)->bool {a.dev==b.dev&&a.ino==b.ino&&a.len==b.len&&a.mode==b.mode&&a.mtime==b.mtime&&a.ctime==b.ctime}
#[derive(Default)]
struct ImportedFiles {bytes:u64,inodes:BTreeMap<(u64,u64),(Stamp,ObjectRef,String)>}
impl ImportedFiles {
    #[cfg(test)]
    fn read(&mut self,file:File,before:&Stamp,store:&LocalStore,stage:&str,original:&ImagePreseedBudget,cancelled:&AtomicBool)->Result<(ObjectRef,String)> {
        let content_store=StagedContentStore::new(store,stage)?;
        self.read_with(file,before,&content_store,original,cancelled)
    }
    fn read_with(&mut self,file:File,before:&Stamp,content_store:&(impl ContentStore+?Sized),original:&ImagePreseedBudget,cancelled:&AtomicBool)->Result<(ObjectRef,String)> {
        clock(original,cancelled)?;
        if let Some((captured,content,sha256))=self.inodes.get(&(before.dev,before.ino)) {
            ensure!(same_inode_version(captured,before),"image hardlink inode changed during import");
            return Ok((content.clone(),sha256.clone()));
        }
        self.bytes=self.bytes.checked_add(before.len).context("image bytes overflow")?;ensure!(self.bytes<=MAX_BYTES,"image unique inode bytes exceed bound");
        let mut reader=HashReader{file,offset:0,length:before.len,sha:Sha256::new(),original,cancelled};
        let content=content::import_file(content_store,&mut reader)?;ensure!(reader.offset==before.len,"image content read truncated");
        let sha256=format!("{:x}",reader.sha.finalize());self.inodes.insert((before.dev,before.ino),(before.clone(),content.clone(),sha256.clone()));Ok((content,sha256))
    }
}
fn stamp(path:&Path)->Result<Stamp>{let m=fs::symlink_metadata(path)?;ensure!(m.uid()==0&&m.gid()==0,"image data contains non-root-owned entry: {}",path.display());
    ensure!(m.file_type().is_symlink()||m.mode()&0o022==0,"image data contains mutable entry: {}",path.display());ensure!(m.is_file()||m.is_dir()||m.file_type().is_symlink(),"image data contains special entry: {}",path.display());
    Ok(Stamp{path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())})}
struct Root {path:PathBuf,file:File,stamp:Stamp}
struct ProviderFile {file:File,stamp:Stamp}
impl ProviderFile {
    fn capture()->Result<Self>{let path=Path::new(PROVIDER_SWAP_TOOL);ensure!(path.canonicalize()?==path,"provider tool redirects");
        for parent in path.parent().unwrap().ancestors(){let s=stamp(parent)?;ensure!(s.mode&libc::S_IFMT==libc::S_IFDIR,"provider tool ancestor is not a directory");}
        let before=stamp(path)?;ensure!(before.mode&libc::S_IFMT==libc::S_IFREG,"provider tool is not a regular file");
        let file=OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path)?;let m=file.metadata()?;
        ensure!((m.dev(),m.ino(),m.len(),m.mode())==(before.dev,before.ino,before.len,before.mode),"provider tool changed during open");Ok(Self{file,stamp:before})}
    fn current(&self)->Result<()>{let m=self.file.metadata()?;ensure!(stamp(&self.stamp.path)?==self.stamp&&(m.dev(),m.ino(),m.len(),m.mode())==(self.stamp.dev,self.stamp.ino,self.stamp.len,self.stamp.mode),"actual held provider tool changed");Ok(())}
}
impl Root {fn capture(path:&Path)->Result<Self>{ensure!(path.canonicalize()?==path,"image root redirects");for parent in path.ancestors(){let s=stamp(parent)?;ensure!(s.mode&libc::S_IFMT==libc::S_IFDIR,"image ancestor is not directory");}
    let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_DIRECTORY).open(path)?;let s=stamp(path)?;let m=file.metadata()?;ensure!((m.dev(),m.ino())==(s.dev,s.ino),"image root open changed");Ok(Self{path:path.into(),file,stamp:s})}
    fn current(&self)->Result<()> {let m=self.file.metadata()?;ensure!(stamp(&self.path)?==self.stamp&&(m.dev(),m.ino())==(self.stamp.dev,self.stamp.ino),"captured image root changed");Ok(())}}
fn clock(original:&ImagePreseedBudget,cancelled:&AtomicBool)->Result<()>{if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original image-data build cancelled");}original.remaining()?;Ok(())}
struct ImportClock {original:ImagePreseedBudget,cancelled:Arc<AtomicBool>}
impl ImageImportFence for ImportClock {fn remaining(&self)->Result<Duration>{clock(&self.original,&self.cancelled)?;self.original.remaining()}}
fn link_allowed(path:&Path)->Result<()> {let target=path.canonicalize().context("image symlink target unavailable")?;
    ensure!(!is_provider_kernel_path(&target),"image symlink enters provider-only kernel data: {}",path.display());
    let declared=WHOLE_DIRS.iter().chain(STATIC_DIRS.iter()).any(|(_,root)|target.starts_with(root))||STATIC_FILES.iter().any(|(_,file)|target==Path::new(file))||target==Path::new("/dev/null");
    ensure!(declared,"image symlink escapes declared immutable inputs: {} -> {}",path.display(),target.display());if path.starts_with(PACKAGE)&&fs::read_link(path)?.is_absolute(){ensure!(!target.starts_with(PACKAGE),"absolute package symlink cannot survive fixed guest relocation");}Ok(())}
/// Fixed profile DATA only. Callers still retain/check their actual image
/// capability and captured physical file/directory owners.
pub fn static_files()->&'static [(&'static str,&'static str)]{&STATIC_FILES}
pub fn static_directories()->&'static [(&'static str,&'static str)]{&STATIC_DIRS}
/// Mutable carrier is staged before any CAS writes. Errors leave actual staging,
/// roots/content refs and named pin intents here for explicit retained recovery.
pub struct ImageDataBuild {compiler:Option<CompilerSource>,store:LocalStore,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>,roots:Vec<Root>,provider_boundary:Option<Root>,provider_tool:Option<ProviderFile>,directories:DirectoryCustody,stamps:Vec<Stamp>,files:Vec<ImageFileData>,
    bulk:Option<LocalImageImport>,bulk_ack:Option<LocalImagePinAck>,
    stage:Option<String>,links:BTreeMap<(u64,u64),(u64,Vec<Vec<String>>)>,entries:usize,imported:ImportedFiles,installer:Option<ObjectRef>,manifest:Option<ObjectRef>,manifest_bytes:Option<Vec<u8>>,pin:String,pin_attempted:bool,pin_ack:bool,attempted:bool,ready:bool}
impl ImageDataBuild {
    pub fn capture_retaining(out:&mut Option<Self>,store:LocalStore,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        clock(&original,&cancelled)?;ensure!(out.is_none(),"original image DATA build already captured");
        *out=Some(Self::staged(store,original,cancelled));
        out.as_mut().unwrap().build_retaining()
    }
    fn staged(store:LocalStore,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Self {
        Self{compiler:None,store,original,cancelled,roots:Vec::new(),provider_boundary:None,provider_tool:None,directories:DirectoryCustody::default(),stamps:Vec::new(),files:Vec::new(),bulk:None,bulk_ack:None,stage:None,links:BTreeMap::new(),entries:0,imported:ImportedFiles::default(),installer:None,manifest:None,manifest_bytes:None,pin:format!("image-data-build:{}",uuid::Uuid::new_v4()),pin_attempted:false,pin_ack:false,attempted:false,ready:false}
    }
    /// Crate-private fixed profile; the public build-only facade accepts no
    /// source path, alternate manifest pin, runtime grant or destination.
    pub(crate) fn capture_compiler_retaining(out:&mut Option<Self>,store:LocalStore,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        clock(&original,&cancelled)?;ensure!(out.is_none(),"original compiler DATA build already captured");
        let mut staged=Self::staged(store,original,cancelled);staged.compiler=Some(CompilerSource::staged_fixed());*out=Some(staged);
        out.as_mut().unwrap().build_retaining()
    }
    /// Separate fixed C1 source; constructor budget/owners and import/sealer
    /// are exactly the existing compiler path, never a generic pin override.
    pub(crate) fn capture_compiler_c1_retaining(out:&mut Option<Self>,store:LocalStore,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<()> {
        clock(&original,&cancelled)?;ensure!(out.is_none(),"original compiler C1 DATA build already captured");
        let mut staged=Self::staged(store,original,cancelled);staged.compiler=Some(CompilerSource::staged_c1_fixed());*out=Some(staged);
        out.as_mut().unwrap().build_retaining()
    }
    #[cfg(test)]
    pub(crate) fn capture_compiler_fixture_retaining(out:&mut Option<Self>,store:LocalStore,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>,source:CompilerSource)->Result<()> {
        ensure!(out.is_none(),"compiler fixture carrier occupied");let mut staged=Self::staged(store,original,cancelled);staged.compiler=Some(source);*out=Some(staged);
        out.as_mut().unwrap().build_retaining()
    }
    pub(crate) fn compiler_binding_data(&self)->Result<CompilerArtifactBindingData> {
        self.pin_data()?;self.compiler.as_ref().context("not a compiler DATA profile")?.binding_data()
    }
    fn build_compiler_retaining(&mut self)->Result<()> {
        self.compiler.as_mut().unwrap().capture(&self.original,&self.cancelled)?;
        let root=self.compiler.as_ref().unwrap().root().to_owned();self.roots.push(Root::capture(&root)?);
        LocalImageImport::stage_retaining(&mut self.bulk,self.store.clone(),Arc::new(ImportClock{original:self.original.clone(),cancelled:self.cancelled.clone()}))?;
        self.stage=Some(self.bulk.as_ref().unwrap().staging_id_data()?);
        let (mode,entries)=self.import_directory_entries(&root,0)?;ensure!(mode==0o555,"compiler installer root mode differs");
        self.compiler.as_ref().unwrap().finish()?;self.roots.last().unwrap().current()?;
        self.finish_retaining(entries)
    }
    fn build_retaining(&mut self)->Result<()> {
        ensure!(!self.attempted,"original image DATA build already attempted");self.attempted=true;clock(&self.original,&self.cancelled)?;
        if self.compiler.is_some(){return self.build_compiler_retaining();}
        // Required baked mountpoint: actual host Root custody is retained even
        // when Fly boot supplies a separate payload below it. Never import it.
        self.provider_boundary=Some(Root::capture(Path::new(PROVIDER_KERNEL_DIRECTORY))?);
        self.provider_tool=Some(ProviderFile::capture()?);
        LocalImageImport::stage_retaining(&mut self.bulk,self.store.clone(),Arc::new(ImportClock{original:self.original.clone(),cancelled:self.cancelled.clone()}))?;
        self.stage=Some(self.bulk.as_ref().unwrap().staging_id_data()?);let mut roots=Vec::new();
        for(name,path)in WHOLE_DIRS.into_iter().chain(STATIC_DIRS){clock(&self.original,&self.cancelled)?;self.roots.push(Root::capture(Path::new(path))?);let directory=self.import_directory(Path::new(path),0)?;
            roots.push(DirectoryEntry::new(name.as_bytes(),EntryNode::Directory{object:directory})?);self.roots.last().unwrap().current()?;}
        for(name,path)in STATIC_FILES {clock(&self.original,&self.cancelled)?;let s=stamp(Path::new(path))?;let node=self.import_entry(Path::new(path),&s,0)?;roots.push(DirectoryEntry::new(name.as_bytes(),node)?);self.stamps.push(s);}
        // Preserve declared merged-/usr aliases as actual original links.
        for(name,target)in [("bin","/usr/bin"),("lib","/usr/lib"),("lib64","/usr/lib64"),("sbin","/usr/sbin")] {
            let path=Path::new("/").join(name);let s=stamp(&path)?;ensure!(s.mode&libc::S_IFMT==libc::S_IFLNK&&path.canonicalize()?==Path::new(target),"merged usr alias differs from declared layout");
            roots.push(DirectoryEntry::new(name.as_bytes(),EntryNode::Symlink{target_hex:hex_bytes(fs::read_link(&path)?.as_os_str().as_encoded_bytes())})?);self.stamps.push(s);
        }
        self.finish_retaining(roots)
    }
    fn finish_retaining(&mut self,mut roots:Vec<DirectoryEntry>)->Result<()> {
        clock(&self.original,&self.cancelled)?;
        let topology=canonical_json(&hardlink_topology(&self.links)?)?;
        let topology_ref=ObjectRef::from_bytes(ObjectKind::Blob,&topology);self.bulk.as_ref().unwrap().receive(&topology_ref,&topology)?;
        roots.push(DirectoryEntry::new(b"hardlinks.json",EntryNode::File{object:topology_ref.clone(),logical_size:topology_ref.size_bytes,mode:0o444})?);
        let installer=PagedDirectory::new(self.bulk.as_ref().unwrap()).build(0o555,roots)?;self.installer=Some(installer.clone());
        self.files.sort_by(|a,b|a.path.as_os_str().as_encoded_bytes().cmp(b.path.as_os_str().as_encoded_bytes()));ensure!(self.files.windows(2).all(|w|w[0].path!=w[1].path),"duplicate full image manifest path");
        let bytes=canonical_json(&ImageManifestData{format_version:1,installer_input:installer.clone(),files:self.files.clone()})?;ensure!(bytes.len()<=MAX_MANIFEST,"full image manifest exceeds bound");
        let manifest=ObjectRef::from_bytes(ObjectKind::Blob,&bytes);self.manifest=Some(manifest.clone());self.manifest_bytes=Some(bytes);self.bulk.as_ref().unwrap().receive_manifest(&manifest,self.manifest_bytes.as_ref().unwrap())?;
        // Pin the complete installer and its separate full manifest. No circular
        // manifest->installer->manifest edge and no OCI self-reference exists.
        self.pin_attempted=true;
        self.bulk_ack=Some(self.bulk.as_ref().unwrap().commit_pins(&self.pin,&installer,&format!("{}:manifest",self.pin),&manifest)?);
        self.pin_ack=true;self.check_deep_for_import()?;clock(&self.original,&self.cancelled)?;self.ready=true;Ok(())
    }
    fn import_directory(&mut self,path:&Path,depth:usize)->Result<ObjectRef>{
        // The runtime provider boundary remains an explicit empty DATA node.
        // Compiler input is the distinct fixed root and cannot reach this path.
        if path==Path::new(PROVIDER_KERNEL_DIRECTORY){
            clock(&self.original,&self.cancelled)?;ensure!(depth<=MAX_DEPTH,"image depth exceeds bound");let before=self.directories.capture(path)?;
            ensure!(before.mode&libc::S_IFMT==libc::S_IFDIR,"image directory changed type");
            self.provider_boundary.as_ref().context("actual provider boundary custody absent")?.current()?;
            return empty_provider_directory_to(self.bulk.as_ref().unwrap());
        }
        let (mode,entries)=self.import_directory_entries(path,depth)?;PagedDirectory::new(self.bulk.as_ref().unwrap()).build(mode,entries)
    }
    fn import_directory_entries(&mut self,path:&Path,depth:usize)->Result<(u32,Vec<DirectoryEntry>)>{
        clock(&self.original,&self.cancelled)?;ensure!(depth<=MAX_DEPTH,"image depth exceeds bound");let before=self.directories.capture(path)?;ensure!(before.mode&libc::S_IFMT==libc::S_IFDIR,"image directory changed type");
        let mut children=Vec::new();for entry in fs::read_dir(path)?{clock(&self.original,&self.cancelled)?;self.entries+=1;ensure!(self.entries<=MAX_ENTRIES,"image namespace exceeds bound");children.push(entry?);}
        children.sort_by(|a,b|a.file_name().as_encoded_bytes().cmp(b.file_name().as_encoded_bytes()));let mut entries=Vec::new();
        for child in children{let p=child.path();let s=stamp(&p)?;let node=self.import_entry(&p,&s,depth+1)?;entries.push(DirectoryEntry::new(child.file_name().as_encoded_bytes(),node)?);if s.mode&libc::S_IFMT!=libc::S_IFDIR{self.stamps.push(s);}}
        self.directories.check_path(path,&before)?;Ok((before.mode&0o7777,entries))
    }
    fn import_entry(&mut self,path:&Path,before:&Stamp,depth:usize)->Result<EntryNode>{clock(&self.original,&self.cancelled)?;
        if let Some(source)=&mut self.compiler {source.observe_entry(path,before.mode,before.len)?;}
        if path==Path::new(PROVIDER_SWAP_TOOL){
            self.provider_tool.as_ref().context("actual provider tool custody absent")?.current()?;
            return empty_provider_file_to(self.bulk.as_ref().unwrap());
        }
        let node=match before.mode&libc::S_IFMT{
            libc::S_IFDIR=>EntryNode::Directory{object:self.import_directory(path,depth)?},
            libc::S_IFLNK=>{link_allowed(path)?;EntryNode::Symlink{target_hex:hex_bytes(fs::read_link(path)?.as_os_str().as_encoded_bytes())}},
            libc::S_IFREG=>{regular_readable(before)?;
                let file=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;let m=file.metadata()?;ensure!((m.dev(),m.ino(),m.len())==(before.dev,before.ino,before.len),"original image file open changed");
                // One physical inode is read once, while every original alias
                // remains in the manifest, stamp checks and hardlink topology.
                let (content,sha256)=self.imported.read_with(file,before,self.bulk.as_ref().unwrap(),&self.original,&self.cancelled)?;
                if let Some(source)=&mut self.compiler {source.observe_file(path,&sha256,before.len)?;}
                let relative=if let Some(source)=&self.compiler {source.installer_path(path)?}else{installer_path(path)?};let links=self.links.entry((m.dev(),m.ino())).or_insert((m.nlink(),Vec::new()));ensure!(links.0==m.nlink(),"image hardlink count changed");links.1.push(relative);
                self.files.push(ImageFileData{path:path.into(),sha256,size_bytes:before.len,content:content.clone()});EntryNode::File{object:content,logical_size:before.len,mode:before.mode&0o7777}},
            _=>anyhow::bail!("unsupported image entry"),};
        if before.mode&libc::S_IFMT==libc::S_IFDIR{self.directories.check_path(path,before)?;}else{ensure!(stamp(path)?==*before,"image entry changed during import");}clock(&self.original,&self.cancelled)?;Ok(node)
    }
    /// Light checks accompany each verified immutable CAS chunk. Root-qualified
    /// source roots, not all tens-thousands names, remain under held descriptors.
    /// Deep checks run before/after whole import and before publication/adoption.
    fn check_source_roots(&self)->Result<()> {if let Some(source)=&self.compiler {source.check_current()?;}for root in &self.roots{root.current()?;}if let Some(boundary)=&self.provider_boundary{boundary.current()?;}if let Some(tool)=&self.provider_tool{tool.current()?;}Ok(())}
    pub fn check_roots(&self)->Result<()> {self.check_source_roots()?;if self.pin_attempted{self.bulk.as_ref().context("original bulk image owner absent")?.assert_pins_current(self.bulk_ack.as_ref().context("image pin commit unknown; retain original bulk owner")?)?;}Ok(())}
    pub fn check_deep(&self)->Result<()> {self.check_roots()?;self.directories.check_all_with(||Ok(()))?;for previous in &self.stamps{ensure!(stamp(&previous.path)?==*previous,"captured image filesystem changed: {}",previous.path.display());}Ok(())}
    pub fn check_deep_for_import(&self)->Result<()> {clock(&self.original,&self.cancelled)?;self.check_source_roots()?;if self.pin_attempted{self.bulk.as_ref().context("original bulk image owner absent")?.assert_pins_current_for_import(self.bulk_ack.as_ref().context("image pin commit unknown; retain original bulk owner")?)?;}self.directories.check_all_with(||clock(&self.original,&self.cancelled))?;for previous in &self.stamps{clock(&self.original,&self.cancelled)?;ensure!(stamp(&previous.path)?==*previous,"captured image filesystem changed: {}",previous.path.display());}clock(&self.original,&self.cancelled)}
    pub fn import_stats_data(&self)->Result<ImageImportStats>{self.bulk.as_ref().context("actual image import absent")?.stats_data()}
    pub fn pin_data(&self)->Result<ImagePreseedPinData>{ensure!(self.ready&&self.pin_ack,"original image build acknowledgment absent");Ok(ImagePreseedPinData{format_version:1,manifest:self.manifest.as_ref().unwrap().clone(),installer_input:self.installer.as_ref().unwrap().clone()})}
    pub fn files_data(&self)->Result<&[ImageFileData]>{ensure!(self.ready&&self.pin_ack,"original image DATA ACK absent");Ok(&self.files)}
    pub fn manifest_data(&self)->Result<ImageManifestData>{Ok(serde_json::from_slice(self.manifest_bytes()?)?)}
    pub fn manifest_bytes(&self)->Result<&[u8]>{ensure!(self.ready&&self.pin_ack,"original image manifest acknowledgment absent");Ok(self.manifest_bytes.as_ref().unwrap())}
    pub fn objects(&self)->Result<Vec<ObjectRef>>{self.pin_data()?;self.bulk.as_ref().context("actual image import absent")?.objects_after_commit(self.bulk_ack.as_ref().context("actual image pin ACK absent")?)}
    pub fn objects_for_import(&self)->Result<Vec<ObjectRef>>{clock(&self.original,&self.cancelled)?;self.pin_data()?;self.bulk.as_ref().context("actual image import absent")?.objects_for_import(self.bulk_ack.as_ref().context("actual image pin ACK absent")?)}

}
#[cfg(test)]
fn empty_provider_directory(store:&LocalStore,stage:&str)->Result<ObjectRef> {empty_provider_directory_to(&LocalDirectoryStore::new(store,stage))}
fn empty_provider_directory_to(store:&(impl DirectoryStore+?Sized))->Result<ObjectRef> {
    // This exact deterministic DATA node is also the actual guest's mandatory
    // empty tmpfs mode. It contains no provider payload/hash/mtime/ABI claim.
    PagedDirectory::new(store).build(PROVIDER_KERNEL_EMPTY_MODE,Vec::new())
}
#[cfg(test)]
fn empty_provider_file(store:&LocalStore,stage:&str)->Result<EntryNode>{empty_provider_file_to(&StagedContentStore::new(store,stage)?)}
fn empty_provider_file_to(store:&(impl ContentStore+?Sized))->Result<EntryNode>{
    let object=store.put_blob(b"")?;
    Ok(EntryNode::File{object,logical_size:0,mode:PROVIDER_SWAP_EMPTY_MODE})
}
fn hardlink_topology(links:&BTreeMap<(u64,u64),(u64,Vec<Vec<String>>)>) -> Result<rudder_engine::tree::HardlinkTopology> {
    let mut groups=Vec::new();for (_, (links,paths))in links {ensure!(*links==paths.len() as u64,"image hardlink has an undeclared alias");if paths.len()>1{let mut paths=paths.clone();paths.sort();groups.push(rudder_engine::tree::HardlinkGroup{paths_hex:paths});}}
    groups.sort_by(|a,b|a.paths_hex.cmp(&b.paths_hex));Ok(rudder_engine::tree::HardlinkTopology{format_version:1,groups})
}
fn regular_readable(before:&Stamp)->Result<()> {ensure!(before.mode&libc::S_IFMT==libc::S_IFREG&&before.mode&0o004!=0,"image regular file is not readable by the unprivileged engine");Ok(())}
#[cfg(test)]
fn graph_objects(store:&LocalStore,installer:&ObjectRef,manifest:&ObjectRef)->Result<Vec<ObjectRef>> {
    ensure!(installer.kind==ObjectKind::Directory&&manifest.kind==ObjectKind::Blob&&manifest.size_bytes<=MAX_MANIFEST as u64,"image installer/manifest type or bound differs");
    let mut objects=BTreeSet::new();let mut total=0u64;
    for root in [installer,manifest]{for reference in store.retained_closure(root)?{if objects.insert(reference.clone()){total=total.checked_add(reference.size_bytes).context("image graph overflow")?;
        ensure!(objects.len()<=MAX_ENTRIES&&total<=MAX_BYTES&&(reference==*manifest||reference.size_bytes<=content::CHUNK_BYTES as u64),"image complete graph exceeds bound");}}}Ok(objects.into_iter().collect())
}
fn installer_path(path:&Path)->Result<Vec<String>> {
    for(name,root)in WHOLE_DIRS.into_iter().chain(STATIC_DIRS){if let Ok(relative)=path.strip_prefix(root){let mut parts=vec![hex_bytes(name.as_bytes())];for part in relative.components(){ensure!(matches!(part,std::path::Component::Normal(_)),"unsafe installer path");parts.push(hex_bytes(part.as_os_str().as_encoded_bytes()));}return Ok(parts);}}
    for(name,file)in STATIC_FILES{if path==Path::new(file){return Ok(vec![hex_bytes(name.as_bytes())]);}}
    anyhow::bail!("file outside fixed image installer roots")
}
fn hex_bytes(bytes:&[u8])->String {const HEX:&[u8]=b"0123456789abcdef";let mut out=String::with_capacity(bytes.len()*2);for b in bytes{out.push(HEX[(b>>4)as usize]as char);out.push(HEX[(b&15)as usize]as char);}out}
struct HashReader<'a>{file:File,offset:u64,length:u64,sha:Sha256,original:&'a ImagePreseedBudget,cancelled:&'a AtomicBool}
impl Read for HashReader<'_>{fn read(&mut self,out:&mut[u8])->std::io::Result<usize>{
    clock(self.original,self.cancelled).map_err(std::io::Error::other)?;if out.is_empty(){return Ok(0);}
    if self.offset==self.length {let mut extra=[0u8;1];let n=self.file.read_at(&mut extra,self.offset)?;clock(self.original,self.cancelled).map_err(std::io::Error::other)?;
        if n!=0{return Err(std::io::Error::other("image file grew beyond captured byte bound"));}return Ok(0);}
    let remaining=usize::try_from((self.length-self.offset).min(64*1024)).unwrap();let limit=out.len().min(remaining);
    let n=self.file.read_at(&mut out[..limit],self.offset)?;self.offset+=n as u64;self.sha.update(&out[..n]);clock(self.original,self.cancelled).map_err(std::io::Error::other)?;Ok(n)
}}

#[cfg(test)]
#[path="image_preseed_data_tests.rs"]
mod tests;

#[cfg(test)]
#[path="image_provider_kernel_boundary_tests.rs"]
mod provider_kernel_tests;

#[cfg(test)]
#[path="image_preseed_bulk_tests.rs"]
mod bulk_tests;
