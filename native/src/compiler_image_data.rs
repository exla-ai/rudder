//! Fixed A2 compiler artifact DATA binding for the build-only image emitter.
//! Original files are hashed in ImageDataBuild's existing chunk-import pass.
//! These canonical bytes, receipts and descriptors grant no runtime authority.
#![cfg(target_os="linux")]
use anyhow::{ensure,Context,Result};
use crate::{engine_registration::ImagePreseedBudget,image_preseed_data::ImageDataBuild};
use rudder_engine::{protocol::{canonical_json,ObjectKind,ObjectRef},store::LocalStore};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use std::{collections::{BTreeMap,BTreeSet},fs::{self,File,OpenOptions},os::unix::{fs::{FileExt,MetadataExt,OpenOptionsExt}},path::{Component,Path,PathBuf},sync::{Arc,atomic::{AtomicBool,Ordering}}};

pub const COMPILER_SOURCE_DIRECTORY:&str="/rudder-compiler-v1";
pub const COMPILER_CAS_DIRECTORY:&str="/var/lib/rudder-compiler-cas-v1";
pub const COMPILER_MANIFEST_SHA256:&str="add56ea6ade5411145b026094f63d8d5838e38b58d397d09749027090d346d0c";
pub const COMPILER_ARTIFACT_IMAGE:&str="registry.fly.io/rudder-workers-exla@sha256:88d25bcd2d359fa6c829a9126d96011b17d197b23307f997e2406e00bc5d562f";
const PROFILE:&str="rudder-linux-rust-typescript-v1";
const MAX_MANIFEST:usize=32*1024*1024;
const MAX_ENTRIES:usize=65_536;
const MAX_BYTES:u64=2*1024*1024*1024;
const A2_ENTRIES:usize=1047;
const A2_BYTES:u64=852_319_618;
const REQUIRED_FILES:[&str;7]=["buck2","prepared/tools/rust/bin/rustc","prepared/tools/node","prepared/tools/typescript/lib/tsc.js","prepared/tools/python/bin/python3","prepared/tools/unshare","prepared/lib64/ld-linux-x86-64.so.2"];
const REQUIRED_DIRS:[&str;4]=["prepared/workspace","prepared/tmp","prepared/dev","prepared/proc"];

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
struct CompilerManifest {format_version:u32,profile_id:String,entries:Vec<CompilerEntry>,logical_bytes:u64}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(tag="kind",deny_unknown_fields)]
enum CompilerEntry {
    #[serde(rename="directory")] Directory {path:String,mode:u32},
    #[serde(rename="file")] File {path:String,mode:u32,size_bytes:u64,sha256:String},
}
impl CompilerEntry {
    fn path(&self)->&str {match self{Self::Directory{path,..}|Self::File{path,..}=>path}}
    fn directory(&self)->bool {matches!(self,Self::Directory{..})}
}
/// DATA only. Its artifact image string records the pinned producer input;
/// only the actual separately authenticated image owner can grant later use.
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct CompilerArtifactBindingData {
    pub format_version:u32,pub profile_id:String,pub artifact_image:String,
    pub artifact_manifest:ObjectRef,pub entries:usize,pub logical_bytes:u64,
}
fn relative(path:&str)->Result<()> {
    ensure!(!path.is_empty()&&path.len()<=1024&&!path.contains('\0')&&!path.starts_with('/')&&!path.ends_with('/')
        &&path.split('/').count()<=32&&path.split('/').all(|part|!part.is_empty()&&part!="."&&part!=".."),"unsafe compiler manifest path");
    ensure!(Path::new(path).components().all(|c|matches!(c,Component::Normal(_))),"compiler manifest path component differs");Ok(())
}
fn parse_manifest(raw:&[u8],expected:&str,exact_a2:bool)->Result<CompilerManifest> {
    ensure!(!raw.is_empty()&&raw.len()<=MAX_MANIFEST,"compiler manifest byte bound");
    ensure!(format!("{:x}",Sha256::digest(raw))==expected,"original compiler manifest pin differs");
    // Comparing the original encoding also refuses duplicate JSON keys,
    // whitespace/newlines and noncanonical numbers before schema conversion.
    let value:serde_json::Value=serde_json::from_slice(raw)?;
    ensure!(canonical_json(&value)?==raw,"compiler manifest is not canonical");
    let manifest:CompilerManifest=serde_json::from_value(value)?;
    ensure!(manifest.format_version==1&&manifest.profile_id==PROFILE&&!manifest.entries.is_empty()&&manifest.entries.len()<=MAX_ENTRIES,"compiler manifest schema/profile/bound");
    let mut seen:BTreeMap<String,bool>=BTreeMap::new();let mut prior:Option<&str>=None;let mut total=0u64;
    for entry in &manifest.entries {
        let path=entry.path();relative(path)?;
        ensure!(path!="manifest.json"&&path!="hardlinks.json"&&prior.is_none_or(|p|p.as_bytes()<path.as_bytes()),"compiler manifest reserved/duplicate/unordered path");
        prior=Some(path);
        if let Some((parent,_))=path.rsplit_once('/') {ensure!(seen.get(parent)==Some(&true),"compiler manifest lacks declared directory parent");}
        match entry {
            CompilerEntry::Directory{mode,..}=>ensure!(*mode==0o555,"compiler manifest directory mode"),
            CompilerEntry::File{mode,size_bytes,sha256,..}=>{
                ensure!(matches!(*mode,0o444|0o555)&&*size_bytes<=MAX_BYTES&&sha256.len()==64
                    &&sha256.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)),"compiler manifest file fields");
                total=total.checked_add(*size_bytes).context("compiler logical byte overflow")?;ensure!(total<=MAX_BYTES,"compiler logical byte bound");
            }
        }
        seen.insert(path.to_owned(),entry.directory());
    }
    ensure!(total==manifest.logical_bytes,"compiler manifest logical byte total differs");
    ensure!(REQUIRED_FILES.iter().all(|p|seen.get(*p)==Some(&false))&&REQUIRED_DIRS.iter().all(|p|seen.get(*p)==Some(&true)),"required compiler layout absent");
    ensure!(seen.iter().any(|(path,dir)|!dir&&path.starts_with("prepared/tools/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/")&&path.ends_with(".rlib")),"compiler standard library closure absent");
    if exact_a2 {ensure!(manifest.entries.len()==A2_ENTRIES&&total==A2_BYTES,"original A2 compiler counts differ");}
    Ok(manifest)
}
#[derive(Clone,Debug,PartialEq,Eq)]
struct Version {dev:u64,ino:u64,len:u64,mode:u32,uid:u32,gid:u32,nlink:u64,mtime:(i64,i64),ctime:(i64,i64)}
impl Version {fn of(m:&fs::Metadata)->Self {Self{dev:m.dev(),ino:m.ino(),len:m.len(),mode:m.mode(),uid:m.uid(),gid:m.gid(),nlink:m.nlink(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec())}}}
fn clock(original:&ImagePreseedBudget,cancelled:&AtomicBool)->Result<()> {if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original compiler CAS capture cancelled");}original.remaining()?;Ok(())}
/// Staged in ImageDataBuild before open/read/CAS effects. Every error retains
/// the original manifest/root FDs and membership/progress; no IO in Drop.
pub(crate) struct CompilerSource {
    root:PathBuf,expected:String,exact_a2:bool,owner:(u32,u32),root_fd:Option<File>,root_version:Option<Version>,
    manifest_fd:Option<File>,manifest_version:Option<Version>,raw:Vec<u8>,manifest:Option<CompilerManifest>,
    expected_entries:BTreeMap<String,CompilerEntry>,seen:BTreeSet<String>,verified:BTreeSet<String>,verified_bytes:u64,
    attempted:bool,ready:bool,
}
impl CompilerSource {
    pub(crate) fn staged_fixed()->Self {Self::staged(PathBuf::from(COMPILER_SOURCE_DIRECTORY),COMPILER_MANIFEST_SHA256.into(),true,(0,0))}
    fn staged(root:PathBuf,expected:String,exact_a2:bool,owner:(u32,u32))->Self {Self{root,expected,exact_a2,owner,root_fd:None,root_version:None,manifest_fd:None,manifest_version:None,raw:Vec::new(),manifest:None,expected_entries:BTreeMap::new(),seen:BTreeSet::new(),verified:BTreeSet::new(),verified_bytes:0,attempted:false,ready:false}}
    pub(crate) fn root(&self)->&Path {&self.root}
    pub(crate) fn capture(&mut self,original:&ImagePreseedBudget,cancelled:&AtomicBool)->Result<()> {
        ensure!(!self.attempted,"original compiler capture cannot retry");self.attempted=true;clock(original,cancelled)?;
        ensure!(self.root.is_absolute()&&self.root.canonicalize()?==self.root,"compiler root redirects");
        for parent in self.root.ancestors(){clock(original,cancelled)?;let m=fs::symlink_metadata(parent)?;
            ensure!(m.is_dir()&&!m.file_type().is_symlink()&&(m.uid(),m.gid())==self.owner&&m.mode()&0o022==0,"compiler source ancestry not immutable owner");}
        let root=fs::symlink_metadata(&self.root)?;ensure!(root.is_dir()&&root.mode()&0o7777==0o555,"compiler root mode differs");self.root_version=Some(Version::of(&root));
        self.root_fd=Some(OpenOptions::new().read(true).custom_flags(libc::O_PATH|libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(&self.root)?);
        ensure!(Version::of(&self.root_fd.as_ref().unwrap().metadata()?)==*self.root_version.as_ref().unwrap(),"compiler root changed at open");
        let path=self.root.join("manifest.json");let before=fs::symlink_metadata(&path)?;
        ensure!(before.is_file()&&!before.file_type().is_symlink()&&(before.uid(),before.gid())==self.owner&&before.mode()&0o7777==0o444&&before.len()<=MAX_MANIFEST as u64,"compiler manifest owner/mode/type/bound");
        self.manifest_version=Some(Version::of(&before));self.manifest_fd=Some(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(&path)?);
        ensure!(Version::of(&self.manifest_fd.as_ref().unwrap().metadata()?)==*self.manifest_version.as_ref().unwrap(),"compiler manifest changed at open");
        self.raw=vec![0;usize::try_from(before.len())?];let mut offset=0usize;
        while offset<self.raw.len(){clock(original,cancelled)?;let end=(offset+64*1024).min(self.raw.len());let n=self.manifest_fd.as_ref().unwrap().read_at(&mut self.raw[offset..end],offset as u64)?;
            ensure!(n>0,"compiler manifest ended early");offset+=n;}
        let mut extra=[0u8;1];ensure!(self.manifest_fd.as_ref().unwrap().read_at(&mut extra,before.len())?==0,"compiler manifest grew");clock(original,cancelled)?;
        let manifest=parse_manifest(&self.raw,&self.expected,self.exact_a2)?;
        self.expected_entries=manifest.entries.iter().cloned().map(|e|(e.path().to_owned(),e)).collect();self.manifest=Some(manifest);
        self.check_held()?;clock(original,cancelled)?;self.ready=true;Ok(())
    }
    fn check_held(&self)->Result<()> {
        for(fd,version,path)in[(&self.root_fd,&self.root_version,self.root.clone()),(&self.manifest_fd,&self.manifest_version,self.root.join("manifest.json"))]{
            let expected=version.as_ref().context("original compiler capture incomplete")?;
            ensure!(Version::of(&fd.as_ref().context("original compiler descriptor absent")?.metadata()?)==*expected&&Version::of(&fs::symlink_metadata(path)?)==*expected,"original compiler descriptor/name changed");
        }Ok(())
    }
    pub(crate) fn check_current(&self)->Result<()> {ensure!(self.ready,"original compiler manifest capture incomplete");self.check_held()}
    fn relative_name<'a>(&self,path:&'a Path)->Result<&'a str> {let path=path.strip_prefix(&self.root).context("compiler input outside fixed root")?;
        let name=path.to_str().context("compiler source path is not UTF8")?;relative(name)?;Ok(name)}
    pub(crate) fn observe_entry(&mut self,path:&Path,mode:u32,len:u64)->Result<()> {
        ensure!(self.ready,"original compiler input unavailable");let name=self.relative_name(path)?.to_owned();
        if name=="manifest.json" {ensure!(mode&libc::S_IFMT==libc::S_IFREG&&mode&0o7777==0o444&&len==self.raw.len()as u64,"original compiler manifest entry changed");return Ok(())}
        let expected=self.expected_entries.get(&name).context("undeclared compiler source member")?;
        match expected {
            CompilerEntry::Directory{mode:expected,..}=>ensure!(mode&libc::S_IFMT==libc::S_IFDIR&&mode&0o7777==*expected,"compiler directory type/mode differs"),
            CompilerEntry::File{mode:expected,size_bytes,..}=>ensure!(mode&libc::S_IFMT==libc::S_IFREG&&mode&0o7777==*expected&&len==*size_bytes,"compiler file type/mode/size differs"),
        }
        ensure!(self.seen.insert(name),"compiler source path observed twice");Ok(())
    }
    pub(crate) fn observe_file(&mut self,path:&Path,sha256:&str,len:u64)->Result<()> {
        let name=self.relative_name(path)?.to_owned();
        if name=="manifest.json" {ensure!(sha256==self.expected&&len==self.raw.len()as u64,"imported original compiler manifest differs");ensure!(self.verified.insert(name),"compiler manifest imported twice");return Ok(())}
        let CompilerEntry::File{sha256:expected,size_bytes,..}=self.expected_entries.get(&name).context("undeclared imported compiler file")? else{anyhow::bail!("compiler directory imported as file")};
        ensure!(self.seen.contains(&name)&&sha256==expected&&len==*size_bytes,"imported compiler raw SHA/size differs: {}",name);
        self.verified_bytes=self.verified_bytes.checked_add(len).context("compiler imported byte overflow")?;
        ensure!(self.verified_bytes<=MAX_BYTES&&self.verified.insert(name),"compiler payload duplicate/byte bound");Ok(())
    }
    pub(crate) fn finish(&self)->Result<()> {self.check_current()?;let manifest=self.manifest.as_ref().unwrap();
        ensure!(self.seen.len()==self.expected_entries.len()&&self.expected_entries.iter().all(|(path,e)|e.directory()||self.verified.contains(path))
            &&self.verified.contains("manifest.json")&&self.verified.len()==self.expected_entries.values().filter(|e|!e.directory()).count()+1
            &&self.verified_bytes==manifest.logical_bytes,"compiler source closure incomplete");Ok(())}
    pub(crate) fn installer_path(&self,path:&Path)->Result<Vec<String>> {let name=self.relative_name(path)?;Ok(name.split('/').map(|p|hex(p.as_bytes())).collect())}
    pub(crate) fn binding_data(&self)->Result<CompilerArtifactBindingData> {self.finish()?;let manifest=self.manifest.as_ref().unwrap();
        Ok(CompilerArtifactBindingData{format_version:1,profile_id:PROFILE.into(),artifact_image:if self.exact_a2{COMPILER_ARTIFACT_IMAGE.into()}else{"unqualified-component-test-fixture".into()},artifact_manifest:ObjectRef::from_bytes(ObjectKind::Blob,&self.raw),entries:manifest.entries.len(),logical_bytes:manifest.logical_bytes})}
    #[cfg(test)]
    pub(crate) fn fixture(root:PathBuf,raw:&[u8])->Self {Self::staged(root,format!("{:x}",Sha256::digest(raw)),false,(0,0))}
}
fn hex(bytes:&[u8])->String {const HEX:&[u8]=b"0123456789abcdef";let mut out=String::with_capacity(bytes.len()*2);for b in bytes{out.push(HEX[(b>>4)as usize]as char);out.push(HEX[(b&15)as usize]as char);}out}

/// Fixed build-only entry: no arbitrary root, expected pin or destination API.
pub fn capture_build_retaining(out:&mut Option<ImageDataBuild>,store:LocalStore,original:ImagePreseedBudget,cancelled:Arc<AtomicBool>)->Result<()> {
    ImageDataBuild::capture_compiler_retaining(out,store,original,cancelled)
}
pub fn binding_data(build:&ImageDataBuild)->Result<CompilerArtifactBindingData> {build.compiler_binding_data()}

#[cfg(test)]
#[path="compiler_image_data_tests.rs"]
mod tests;
