//! Explicit V2 seed bytes. This does not mint accepted-source, Root-read or
//! task authority. The caller retains the actual quiesced repository/lease.
use super::*;
use rudder_engine::{content::{self, StagedContentStore}, directory::{self, LocalDirectoryStore,
    PagedDirectory, DirectoryEntry, EntryNode}, execution::finalization_budget::FinalizationBudget,
    store::LocalStore};
use std::{os::unix::fs::{MetadataExt, OpenOptionsExt}, sync::{Arc, Mutex,
    atomic::{AtomicBool, Ordering}}};

/// V1 remains the default. Selecting V2 is DATA, never a Root qualification ACK.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum SeedProfile { #[default] SmallV1, PagedCowV2 }
/// Fixed, versioned qualification envelope, derived from the current Git import
/// and metadata-localization profiles. It does not widen Git import limits.
pub const V2_MAX_METADATA_FILES: usize = 32_768;
pub const V2_MAX_OBJECT_FILES: usize = 100_000;
pub const V2_MAX_FILES: usize = V2_MAX_METADATA_FILES + V2_MAX_OBJECT_FILES;
pub const V2_MAX_LOGICAL_BYTES: u64 = 512 << 20;
pub const V2_MAX_METADATA_BYTES: u64 = 128 << 20;
pub const V2_MAX_OBJECT_BYTES: u64 = 384 << 20;
pub const V2_MAX_FILE_BYTES: u64 = 128 << 20;
pub const V2_MAX_REFS: usize = 262_144;
pub const V2_MAX_CAS_BYTES: u64 = 768 << 20;
pub const V2_MAX_DESCRIPTOR_BYTES: usize = 4096;
pub const V2_RECORDS_PER_PAGE: usize = 128;
pub const V2_MAX_PAGE_BYTES: usize = 128 << 10;
pub const V2_PATH_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PagedSeedDescriptor {
    pub format_version:u32, pub jj_version:String, pub baseline:CommitIdentity, pub operation:JjOperationId,
    pub metadata:ObjectRef, pub objects:ObjectRef, pub private:ObjectRef, pub records:ObjectRef,
    pub metadata_files:u64, pub object_files:u64, pub metadata_bytes:u64, pub object_bytes:u64,
}
impl PagedSeedDescriptor {
    pub fn validate(&self)->Result<()> {
        ensure!(self.format_version==2&&self.jj_version=="0.40.0","unsupported paged seed profile");
        self.baseline.validate()?;self.operation.validate()?;
        for r in [&self.metadata,&self.objects,&self.private,&self.records] {r.validate()?;
            ensure!(r.kind==ObjectKind::Directory&&r.size_bytes<=4096,"paged seed root must be compact DirectoryV2");}
        ensure!(self.metadata_files>0&&self.metadata_files<=V2_MAX_METADATA_FILES as u64
            &&self.object_files<=V2_MAX_OBJECT_FILES as u64&&self.metadata_bytes<=V2_MAX_METADATA_BYTES
            &&self.object_bytes<=V2_MAX_OBJECT_BYTES,"paged seed exceeds qualified profile");Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PagedSeedFile {pub path:String,pub object:ObjectRef,pub sha256:String,pub size_bytes:u64}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordsPage {format_version:u32,files:Vec<PagedSeedFile>}
fn path(name:&str)->Result<()> {
    ensure!(!name.is_empty()&&name.len()<=512&&Path::new(name).components().count()<=V2_PATH_DEPTH
        &&name.split('/').all(|p|!p.is_empty()&&p!="."&&p!="..")
        &&name.bytes().all(|b|b.is_ascii_alphanumeric()||b"._-/".contains(&b)),"paged seed path unsupported");Ok(())
}
fn file_record(record:&PagedSeedFile)->Result<()> {
    path(&record.path)?;record.object.validate()?;
    ensure!(record.size_bytes<=V2_MAX_FILE_BYTES&&matches!(record.object.kind,ObjectKind::Blob|ObjectKind::File)
        &&record.sha256.len()==64&&record.sha256.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)),"invalid paged seed file");
    if record.object.kind==ObjectKind::Blob {ensure!(record.object.size_bytes==record.size_bytes&&record.object.digest==record.sha256,"blob descriptor differs");}
    Ok(())
}
/// Decode bounded pages and cross-check the actual content trees, not just a
/// payload-supplied filename list. The graph is already pinned by the owner.
pub fn read_paged_seed(store:&LocalStore,root:&ObjectRef,mut check:impl FnMut()->Result<()>)
    ->Result<(PagedSeedDescriptor,ObjectRef,MetadataTemplate)> {
    let(descriptor,object)=read_paged_descriptor(store,root,&mut check)?;
    let mut metadata=BTreeMap::new();let mut objects=BTreeMap::new();let mut after=None;let mut last=String::new();
    let mut metadata_bytes=0u64;let mut object_bytes=0u64;let mut pages=0;
    loop {check()?;let page=rudder_engine::tree::scan(store,&descriptor.records,after.as_deref(),128)?;
        for e in page.entries {pages+=1;ensure!(pages<=V2_MAX_FILES.div_ceil(V2_RECORDS_PER_PAGE),"too many seed record pages");
            let name=String::from_utf8(e.name()?)?;ensure!(name==format!("{:08}",pages-1),"seed record page order differs");
            let EntryNode::File {object,logical_size,..}=e.node else {anyhow::bail!("seed record page is not a file")};
            ensure!(object.kind==ObjectKind::Blob&&logical_size<=V2_MAX_PAGE_BYTES as u64,"seed record page too large");
            let bytes=store.read_object(&object)?;let p:RecordsPage=serde_json::from_slice(&bytes)?;
            ensure!(p.format_version==2&&!p.files.is_empty()&&p.files.len()<=V2_RECORDS_PER_PAGE&&canonical_json(&p)?==bytes,"invalid seed record page");
            for f in p.files {check()?;file_record(&f)?;ensure!(last<f.path,"seed records duplicate or unordered");last=f.path.clone();
                let (base,name,map,sum)=if let Some(name)=f.path.strip_prefix("metadata/"){(&descriptor.metadata,name,&mut metadata,&mut metadata_bytes)}
                    else if let Some(name)=f.path.strip_prefix("objects/"){(&descriptor.objects,name,&mut objects,&mut object_bytes)}
                    else {anyhow::bail!("seed record outside namespace")};
                let entry=lookup_path(store,base,name)?.context("seed record has no content")?;
                ensure!(matches!(entry.node,EntryNode::File{object,logical_size,mode} if object==f.object&&logical_size==f.size_bytes&&mode==0o600),"seed record content differs");
                ensure!(map.insert(name.to_owned(),SeedFile{sha256:f.sha256,size_bytes:f.size_bytes}).is_none(),"duplicate seed record");
                *sum=sum.checked_add(f.size_bytes).context("seed byte overflow")?;
                ensure!(metadata.len()<=V2_MAX_METADATA_FILES&&objects.len()<=V2_MAX_OBJECT_FILES,"seed file count exceeded");
            }
        }
        after=page.next_after;if after.is_none(){break;}
    }
    ensure!(metadata.len() as u64==descriptor.metadata_files&&objects.len() as u64==descriptor.object_files
        &&metadata_bytes==descriptor.metadata_bytes&&object_bytes==descriptor.object_bytes,"seed summaries differ");
    let manifest=MetadataTemplate{format_version:1,jj_version:descriptor.jj_version.clone(),baseline:descriptor.baseline.clone(),operation:descriptor.operation.clone(),metadata,objects};
    manifest.validate_with_limit(V2_MAX_FILES)?;
    // Detect extra content and certify the private CoW root is the exact graft.
    validate_tree(store,&descriptor.metadata,"metadata",&manifest.metadata,&mut check)?;
    validate_tree(store,&descriptor.objects,"objects",&manifest.objects,&mut check)?;
    let private=private_root(store,&descriptor.metadata,&descriptor.objects,None)?;
    ensure!(private==descriptor.private,"private metadata/object graft differs");check()?;Ok((descriptor,object,manifest))
}
/// Compact immutable header DATA. Production Root calls this only while
/// retaining the SAME seed pin that its original full validator acknowledged.
/// It is not a fresh full integrity scrub or source/Root-ready authority.
pub fn read_paged_descriptor(store:&LocalStore,root:&ObjectRef,mut check:impl FnMut()->Result<()>)->Result<(PagedSeedDescriptor,ObjectRef)>{
    check()?;ensure!(root.kind==ObjectKind::Directory&&root.size_bytes<=4096,"paged seed root exceeds profile");
    let e=rudder_engine::tree::lookup(store,root,b"descriptor.json")?.context("paged seed descriptor absent")?;
    let EntryNode::File {object,logical_size,..}=e.node else {anyhow::bail!("paged seed descriptor is not a file")};
    ensure!(object.kind==ObjectKind::Blob&&logical_size<=V2_MAX_DESCRIPTOR_BYTES as u64,"paged seed descriptor exceeds profile");
    let bytes=store.read_object(&object)?;let descriptor:PagedSeedDescriptor=serde_json::from_slice(&bytes)?;
    descriptor.validate()?;ensure!(canonical_json(&descriptor)?==bytes,"paged seed descriptor is not canonical");
    let mut top=BTreeMap::new();let page=rudder_engine::tree::scan(store,root,None,8)?;
    ensure!(page.next_after.is_none(),"paged seed has extra root entries");
    for entry in page.entries {top.insert(String::from_utf8(entry.name()?)?,entry.node);}
    for (name,expected) in [("metadata",&descriptor.metadata),("objects",&descriptor.objects),
        ("private",&descriptor.private),("records",&descriptor.records)] {
        ensure!(matches!(top.remove(name),Some(EntryNode::Directory{object}) if &object==expected),"paged seed named root differs");}
    ensure!(top.len()==1&&top.contains_key("descriptor.json"),"unexpected paged seed root");
    check()?;Ok((descriptor,object))
}
fn lookup_path(store:&LocalStore,root:&ObjectRef,path:&str)->Result<Option<directory::DirectoryEntry>> {
    let mut root=root.clone();let mut parts=path.split('/').peekable();
    while let Some(name)=parts.next(){let Some(entry)=rudder_engine::tree::lookup(store,&root,name.as_bytes())? else{return Ok(None)};
        if parts.peek().is_none(){return Ok(Some(entry))}
        let EntryNode::Directory{object}=entry.node else{anyhow::bail!("seed path crosses regular file")};root=object;
    }Ok(None)
}
fn validate_tree(store:&LocalStore,root:&ObjectRef,_prefix:&str,files:&BTreeMap<String,SeedFile>,check:&mut impl FnMut()->Result<()>)->Result<()> {
    let mut pending=vec![(String::new(),root.clone())];let mut found=0;let mut entries=0;
    while let Some((prefix,root))=pending.pop(){check()?;let mut after=None;
        loop{let page=rudder_engine::tree::scan(store,&root,after.as_deref(),128)?;
            for e in page.entries {entries+=1;ensure!(entries<=V2_MAX_FILES*3,"seed namespace exceeds profile");
                let name=String::from_utf8(e.name()?)?;let rel=if prefix.is_empty(){name}else{format!("{prefix}/{name}")};path(&rel)?;
                match e.node {EntryNode::Directory{object}=>pending.push((rel,object)),EntryNode::File{logical_size,mode,..}=>{
                    ensure!(files.get(&rel).is_some_and(|f|f.size_bytes==logical_size)&&mode==0o600,"extra or invalid seed file");found+=1;},
                    _=>anyhow::bail!("seed symlink unsupported")}
            }after=page.next_after;if after.is_none(){break;}}
    }ensure!(found==files.len(),"seed tree incomplete");Ok(())
}
/// Build/read the tiny private root graft. Read-only validation computes the
/// same refs without mutating CAS using an in-memory DirectoryStore adapter.
pub(super) fn private_root(store:&LocalStore,metadata:&ObjectRef,objects:&ObjectRef,stage:Option<&str>)->Result<ObjectRef>{
    struct Overlay<'a>{store:&'a LocalStore,pending:Mutex<BTreeMap<ObjectRef,Vec<u8>>>,stage:Option<&'a str>}
    impl directory::DirectoryStore for Overlay<'_>{
        fn read(&self,r:&ObjectRef)->Result<Vec<u8>>{if let Some(b)=self.pending.lock().unwrap().get(r){return Ok(b.clone())}self.store.read_object(r)}
        fn put(&self,k:ObjectKind,b:&[u8])->Result<ObjectRef>{let r=ObjectRef::from_bytes(k,b);if let Some(s)=self.stage{self.store.receive_staged(s,&r,b)?;}self.pending.lock().unwrap().insert(r.clone(),b.to_vec());Ok(r)}
    }
    let adapter=Overlay{store,pending:Mutex::new(BTreeMap::new()),stage};let directory=PagedDirectory::new(&adapter);
    let git=directory.lookup(metadata,b".git")?.context("metadata Git directory absent")?;
    let EntryNode::Directory{object:git}=git.node else{anyhow::bail!("Git metadata root not Directory")};
    ensure!(directory.lookup(&git,b"objects")?.is_none(),"metadata carries unexpected Git objects");
    let git=directory.update(&git,b"objects",Some(EntryNode::Directory{object:objects.clone()}))?.root;
    Ok(directory.update(metadata,b".git",Some(EntryNode::Directory{object:git}))?.root)
}


#[cfg(feature="jj")]
#[path="seed_paged_builder.rs"]
mod builder;
#[cfg(feature="jj")]
pub use builder::{PagedSeedGeneration,PagedSeedWork};
#[cfg(feature="jj")]
pub(crate) use builder::PagedSnapshot;

#[cfg(all(test,feature="jj"))]
#[path="seed_paged_tests.rs"]
mod tests;
