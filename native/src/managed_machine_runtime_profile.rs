//! Actual image/config/CAS custody for explicit managed ingress. The manifest
//! and paths are DATA: this retained owner opens/checks the real fixed layout.
//! No user-selected executable, updater path, model readiness or OAuth fallback.
#![cfg(target_os="linux")]
use crate::{managed_profile_loader::ManagedProfileLoaderRuntime,managed_task_launch::{ManagedTaskPrograms,ManagedTaskResources}};
use anyhow::{ensure,Context,Result};
use rudder_engine::{execution::{ReadOnlyRuntime,finalization_budget::FinalizationBudget},protocol::{ObjectKind,ObjectRef,canonical_json},
    snapshot::EnvironmentV2,store::{LocalStore,DirectoryManifest}};
use rudder_native::host_ingress::{HostIngressConfigOwner,HostRuntimeProfile,HostBackend,AllowedPreparedProject,ImageRuntimeEntry};
use serde::{Serialize,Deserialize};
use sha2::{Digest,Sha256};
use std::{collections::{BTreeMap,BTreeSet},fs::{self,File,OpenOptions},io::Read,os::unix::fs::{MetadataExt,OpenOptionsExt,FileExt},
    path::{Path,PathBuf},sync::{Arc,atomic::{AtomicBool,Ordering}}};
mod budget_sealed {pub trait Sealed {}impl Sealed for rudder_engine::execution::finalization_budget::FinalizationBudget {}impl Sealed for rudder_native::engine_registration::ImagePreseedBudget {}}
pub(crate) trait RuntimeReadBudget:budget_sealed::Sealed {fn remaining(&self)->Result<std::time::Duration>;fn cancel(&self);}
impl RuntimeReadBudget for FinalizationBudget {fn remaining(&self)->Result<std::time::Duration>{FinalizationBudget::remaining(self)}fn cancel(&self){FinalizationBudget::cancel(self)}}
impl RuntimeReadBudget for rudder_native::engine_registration::ImagePreseedBudget {fn remaining(&self)->Result<std::time::Duration>{rudder_native::engine_registration::ImagePreseedBudget::remaining(self)}fn cancel(&self){rudder_native::engine_registration::ImagePreseedBudget::cancel(self)}}
const ROOT:&str="/opt/rudder-managed-runtime/v1";
const GUEST_PACKAGE:&str="/opt/runtime/rudder";
const GUEST_MODEL:&str="/opt/runtime/model/v1";
const NATIVE:&str="/opt/rudder/dist/native/rudder-native";
const JJ:&str="/opt/rudder/dist/native/rudder-jj";
const NODE:&str="/usr/local/bin/node";
const CLI:&str="/opt/rudder/dist/index.js";
const PROFILE:&str="/opt/rudder/dist/managed-task-profile-cli.js";
const MAX_FILES:usize=32768;
const MAX_BYTES:u64=1024*1024*1024;
#[derive(Clone,PartialEq,Eq,Serialize)]
struct Stamp {path:PathBuf,dev:u64,ino:u64,len:u64,mtime:(i64,i64),ctime:(i64,i64),mode:u32}
fn stamp(path:&Path)->Result<Stamp> {let m=fs::symlink_metadata(path)?;ensure!(!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"runtime object is not immutable host image custody");
    Ok(Stamp {path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec()),mode:m.mode()})}
// Capture every real non-symlink ancestor, not just the selected filename.
// Root can mutate the image; the retained snapshots detect such changes at
// each defined validation point and never claim atomicity against a later one.
fn ancestors(path:&Path)->Result<Vec<Arc<DirectoryAnchor>>> {ancestors_anchored(path,&mut DirectoryAnchors::default())}
fn ancestors_anchored(path:&Path,anchors:&mut DirectoryAnchors)->Result<Vec<Arc<DirectoryAnchor>>> {
    ensure!(path.is_absolute(),"image path must be absolute");let mut result=Vec::new();
    for parent in path.ancestors().skip(1) {ensure!(result.len()<32,"image ancestor depth exceeds bound");
        result.push(anchors.capture(parent)?);}
    Ok(result)
}
fn ancestors_current(saved:&[Arc<DirectoryAnchor>])->Result<()> {for prior in saved {prior.current()?;}Ok(())}
struct ImageFile {held:File,stamp:Stamp,sha:String,ancestors:Vec<Arc<DirectoryAnchor>>}
impl ImageFile {
    fn open<B:RuntimeReadBudget+?Sized>(path:&Path,limit:u64,original:&B,cancelled:&AtomicBool)->Result<Self> {Self::open_anchored(path,limit,original,cancelled,&mut DirectoryAnchors::default())}
    fn open_anchored<B:RuntimeReadBudget+?Sized>(path:&Path,limit:u64,original:&B,cancelled:&AtomicBool,anchors:&mut DirectoryAnchors)->Result<Self> {
        clock(original,cancelled)?;let ancestors=ancestors_anchored(path,anchors)?;let before=stamp(path)?;ensure!(before.mode&libc::S_IFMT==libc::S_IFREG&&before.len<=limit,"runtime file size/type unsupported");
        let mut held=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
        let m=held.metadata()?;ensure!((m.dev(),m.ino())==(before.dev,before.ino),"runtime file open changed");
        let mut hash=Sha256::new();let mut chunk=[0u8;64*1024];let mut bytes=0u64;
        loop {clock(original,cancelled)?;let n=held.read(&mut chunk)?;if n==0{break;}bytes=bytes.checked_add(n as u64).context("runtime byte overflow")?;ensure!(bytes<=limit,"runtime file grew");hash.update(&chunk[..n]);}
        ensure!(bytes==before.len&&stamp(path)?==before,"runtime image changed while hashing");
        ancestors_current(&ancestors)?;Ok(Self {held,stamp:before,sha:format!("{:x}",hash.finalize()),ancestors})
    }
    fn current(&self)->Result<()> {ancestors_current(&self.ancestors)?;ensure!(stamp(&self.stamp.path)?==self.stamp,"original image file changed");let m=self.held.metadata()?;
        ensure!((m.dev(),m.ino(),m.len())==(self.stamp.dev,self.stamp.ino,self.stamp.len),"held image file changed");Ok(())}
}
struct Directory {held:Arc<DirectoryAnchor>,stamp:Stamp,ancestors:Vec<Arc<DirectoryAnchor>>}
impl Directory {
    fn open(path:&Path)->Result<Self> {Self::open_anchored(path,&mut DirectoryAnchors::default())}
    fn open_anchored(path:&Path,anchors:&mut DirectoryAnchors)->Result<Self> {
        let ancestors=ancestors_anchored(path,anchors)?;let s=stamp(path)?;ensure!(s.mode&libc::S_IFMT==libc::S_IFDIR&&path.canonicalize()?==path,"image directory redirects");
        let held=anchors.capture_expected(path,&s)?;ancestors_current(&ancestors)?;Ok(Self{held,stamp:s,ancestors})
    }
    fn current(&self)->Result<()> {ancestors_current(&self.ancestors)?;ensure!(self.held.stamp==self.stamp,"original directory anchor changed");self.held.current()}
}
#[path="managed_runtime_directory_anchor.rs"]
mod directory_anchor;
use directory_anchor::{DirectoryAnchor,DirectoryAnchors};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {version:u32,profile_id:String,entries:Vec<ImageRuntimeEntry>}
/// Readonly custody for the metadata-only selector BEFORE any project transfer.
/// The helper's entire actual package is protected before it can import code;
/// this owner opens no CAS and stages no source/task/seed effect.
pub(crate) struct ManagedProfileInspection {config:Arc<HostIngressConfigOwner>,files:Vec<ImageFile>,anchors:DirectoryAnchors,stamps:Vec<Stamp>,hashes:Vec<(PathBuf,String,u32)>,home:PathBuf,
    fresh:Option<Arc<fresh_environment::ManagedFreshEnvironment>>,dirs:Vec<Directory>,loader:Option<OsLoaderInputs>}
impl ManagedProfileInspection {
    pub(crate) fn capture_retaining(out:&mut Option<Self>,config:Arc<HostIngressConfigOwner>,alias:&str,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        ensure!(out.is_none(),"original metadata selector already captured");config.assert_current()?;clock(original,cancelled)?;
        crate::managed_app::runtime_task::require_legacy_environment_backing(&config)?;
        let home=config.data().projects.iter().find(|p|p.alias==alias).context("selector project outside original config")?.home.clone();
        *out=Some(Self{config,files:Vec::new(),anchors:DirectoryAnchors::default(),stamps:Vec::new(),hashes:Vec::new(),home,fresh:None,dirs:Vec::new(),loader:None});let owner=out.as_mut().unwrap();
        for name in [NODE,PROFILE] {owner.files.push(ImageFile::open_anchored(Path::new(name),512*1024*1024,original,cancelled,&mut owner.anchors)?);}
        capture_package(Path::new("/opt/rudder"),&mut owner.stamps,&mut owner.hashes,original,cancelled,&mut owner.anchors)?;owner.current(original,cancelled)
    }
    pub(crate) fn current(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        clock(original,cancelled)?;self.config.assert_current()?;if let Some(fresh)=&self.fresh{fresh.current(original,cancelled)?;}for directory in &self.dirs{directory.current()?;}if let Some(loader)=&self.loader{loader.current()?;}self.anchors.check_all_with(||clock(original,cancelled))?;for f in &self.files {f.current()?;}for before in &self.stamps {ensure!(package_stamp(&before.path)?==*before,"original selector package changed");}clock(original,cancelled)
    }
    pub(crate) fn loader(&self)->ManagedProfileLoaderRuntime {ManagedProfileLoaderRuntime{node:NODE.into(),helper:PROFILE.into(),rudder_home:self.home.clone()}}
}
/// Root owns this through all tasks/Root seed users. No Drop unpin/disposal.
/// Image directory policy forbids writes by the UID1500 engine/guests. Current
/// checks are observational; no atomic guarantee against later privileged Root
/// image mutation is asserted. The fixed Root image owner must keep it immutable.
pub(crate) struct ManagedHostRuntime {
    config:Arc<HostIngressConfigOwner>,project:AllowedPreparedProject,fresh:Option<Arc<fresh_environment::ManagedFreshEnvironment>>,files:Vec<ImageFile>,dirs:Vec<Directory>,anchors:DirectoryAnchors,
    network:Option<VmNetworkInputs>,loader:Option<OsLoaderInputs>,package_stamps:Vec<Stamp>,package_hashes:Vec<(PathBuf,String,u32)>,manifest_hash:String,store:Option<LocalStore>,cas_dir:Option<File>,cas_id:Option<(u64,u64)>,
    environment:Option<ObjectRef>,pin:String,pin_attempted:bool,environment_ack:bool,fenced:AtomicBool,
}
impl ManagedHostRuntime {
    pub(crate) fn capture_retaining(out:&mut Option<Self>,config:Arc<HostIngressConfigOwner>,alias:&str,
        original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        clock(original,cancelled)?;config.assert_current()?;ensure!(out.is_none(),"runtime owner already captured");
        crate::managed_app::runtime_task::require_legacy_environment_backing(&config)?;
        let project=config.data().projects.iter().find(|p|p.alias==alias).context("runtime project outside actual config")?.clone();
        *out=Some(Self {config,project,fresh:None,files:Vec::new(),dirs:Vec::new(),anchors:DirectoryAnchors::default(),network:None,loader:None,package_stamps:Vec::new(),package_hashes:Vec::new(),manifest_hash:String::new(),
            store:None,cas_dir:None,cas_id:None,environment:None,pin:format!("managed-ingress-env:{}",uuid::Uuid::new_v4()),pin_attempted:false,environment_ack:false,fenced:AtomicBool::new(false)});
        out.as_mut().unwrap().capture_inner(original,cancelled)
    }
    pub(crate) fn capture_from_fresh_retaining(out:&mut Option<Self>,fresh:Arc<fresh_environment::ManagedFreshEnvironment>,alias:&str,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        clock(original,cancelled)?;ensure!(out.is_none(),"actual fresh runtime output occupied");let config=fresh.machine().config().clone();
        if fresh.runtime_backing().is_none(){crate::managed_app::runtime_task::require_legacy_environment_backing(&config)?;}fresh.current(original,cancelled)?;
        let project=config.data().projects.iter().find(|p|p.alias==alias&&p.prepare==*fresh.preparation().prepare()).context("fresh runtime outside actual original preparation")?.clone();
        let store=fresh.machine().store()?.clone();let environment=fresh.environment()?.clone();
        *out=Some(Self{config,project,fresh:Some(fresh),files:Vec::new(),dirs:Vec::new(),anchors:DirectoryAnchors::default(),network:None,loader:None,package_stamps:Vec::new(),package_hashes:Vec::new(),manifest_hash:String::new(),store:Some(store),cas_dir:None,cas_id:None,
            environment:Some(environment),pin:String::new(),pin_attempted:false,environment_ack:false,fenced:AtomicBool::new(false)});
        let held=out.as_mut().unwrap();if held.fresh.as_ref().is_some_and(|f|f.runtime_backing().is_some()){return held.capture_backed_fresh_host(original,cancelled)}
        held.network=Some(VmNetworkInputs::capture_anchored(original,cancelled,&mut held.anchors)?);held.assert_current(original,cancelled)?;held.environment_ack=true;Ok(())
    }
    fn capture_inner(&mut self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        self.manifest_hash=capture_fixed_image(&self.config,&mut self.anchors,&mut self.files,&mut self.dirs,&mut self.package_stamps,&mut self.package_hashes,&mut self.loader,original,cancelled)?;
        self.network=Some(VmNetworkInputs::capture_anchored(original,cancelled,&mut self.anchors)?);
        for protected in std::iter::once(&self.config.data().state_directory).chain(self.config.data().projects.iter().flat_map(|p|[&p.checkout,&p.home,&p.cas,&p.endpoints])) {
            for visible in [Path::new("/usr"),Path::new("/opt/rudder"),Path::new(ROOT)] {ensure!(!protected.starts_with(visible)&&!visible.starts_with(protected),"guest runtime overlaps actual host authority/private namespace");}
        }
        let m=fs::symlink_metadata(&self.project.cas)?;ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==1500&&m.gid()==1500&&m.mode()&0o7777==0o700&&self.project.cas.canonicalize()?==self.project.cas,"explicit private CAS missing/changed");
        self.cas_dir=Some(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(&self.project.cas)?);self.cas_id=Some((m.dev(),m.ino()));
        self.store=Some(LocalStore::open(&self.project.cas)?);
        self.assert_current(original,cancelled)?;
        let store=self.store.as_ref().unwrap();let empty=store.put_directory(DirectoryManifest {format_version:1,mode:0o755,entries:Vec::new()})?;
        self.environment=Some(store.put_metadata(ObjectKind::EnvironmentRoot,&EnvironmentV2 {format_version:2,directory:empty,
            platform:BTreeMap::from([("os".into(),"linux".into()),("arch".into(),std::env::consts::ARCH.into()),("runtime_profile".into(),"fly-managed-image-v1".into())]),
            installer_inputs:Vec::new(),provenance:BTreeMap::from([("actual_image_closure_sha256".into(),self.manifest_hash.clone()),("vm_network_inputs_sha256".into(),self.network.as_ref().unwrap().digest.clone())])})?);
        ensure!(!store.retention()?.pins.contains_key(&self.pin),"original environment pin collided");self.pin_attempted=true;store.pin(&self.pin,self.environment.as_ref().unwrap())?;
        self.assert_current(original,cancelled)?;self.environment_ack=true;Ok(())
    }
    pub(crate) fn assert_current(&self,original:&FinalizationBudget,cancelled:&AtomicBool)->Result<()> {
        clock(original,cancelled)?;
        if let Some(fresh)=&self.fresh{loop{clock(original,cancelled)?;match fresh.assert_retained_custody(){Ok(())=>break,
            Err(error)if error.downcast_ref::<crate::engine_host::EngineSharedAdmissionBusy>().is_some()||runtime_environment::is_busy(&error)=>std::thread::sleep(original.phase_limit(std::time::Duration::from_millis(1))?),
            Err(error)=>{self.fenced.store(true,Ordering::Release);return Err(error)}}}}
        let result=(||->Result<()> {ensure!(!self.fenced.load(Ordering::Acquire),"actual runtime owner fenced");self.config.assert_current()?;
            self.anchors.check_all_with(||clock(original,cancelled))?;for dir in &self.dirs {dir.current()?;}for file in &self.files {file.current()?;}if let Some(network)=&self.network {network.current()?;}if let Some(loader)=&self.loader {loader.current()?;}for previous in &self.package_stamps {ensure!(package_stamp(&previous.path)?==*previous,"fixed package closure changed");}
            if let Some(cas)=&self.cas_dir {let m=cas.metadata()?;let n=fs::symlink_metadata(&self.project.cas)?;ensure!(m.is_dir()&&n.is_dir()&&!n.file_type().is_symlink()&&Some((m.dev(),m.ino()))==self.cas_id&&Some((n.dev(),n.ino()))==self.cas_id&&n.uid()==1500&&n.mode()&0o7777==0o700,"original CAS directory changed");}
            if self.pin_attempted {ensure!(self.store.as_ref().unwrap().retention()?.pins.get(&self.pin)==self.environment.as_ref(),"original environment pin absent/changed");}
            Ok(())})();if result.is_err(){self.fenced.store(true,Ordering::Release);}result?;clock(original,cancelled)
    }
    pub(crate) fn store(&self)->&LocalStore {self.store.as_ref().expect("ready runtime store")}
    pub(crate) fn environment(&self)->Result<&ObjectRef> {ensure!(self.environment_ack,"original environment pin ACK unavailable");self.environment.as_ref().context("actual environment absent")}
    pub(crate) fn profile_loader(&self)->ManagedProfileLoaderRuntime {ManagedProfileLoaderRuntime {node:NODE.into(),helper:PROFILE.into(),rudder_home:self.project.home.clone()}}
    pub(crate) fn selected_model_profile(&self, selected:&crate::managed_task_launch::ManagedTaskOptIn,
        model:&str, original:&FinalizationBudget,cancelled:&AtomicBool)->Result<crate::managed_task_launch::SelectedTaskProfile> {
        self.assert_current(original,cancelled)?;
        // Same immutable host artifact whose guest path programs() supplies.
        let backend=match selected.backend() {crate::Backend::Codex=>HostBackend::Codex,crate::Backend::Claude=>HostBackend::Claude,_=>anyhow::bail!("unsupported selected managed backend")};
        self.programs(backend)?;
        let profile=crate::managed_profile_loader::discover_and_load_selected_model(&self.profile_loader(),selected,model,
            &Path::new(ROOT).join("codex"),cancelled,original)?;
        self.assert_current(original,cancelled)?;Ok(profile)
    }
    pub(crate) fn programs(&self,backend:HostBackend)->Result<ManagedTaskPrograms> {
        ensure!(!matches!(backend,HostBackend::Claude),"Claude managed guest/profile qualification is not admitted by this image policy");
        let file=Path::new(ROOT).join(match backend {HostBackend::Codex=>"codex",HostBackend::Claude=>"claude"});
        ensure!(self.files.iter().any(|f|f.stamp.path==file&&f.stamp.mode&0o111!=0)||self.fresh.as_ref().is_some_and(|f|f.image().contains_actual_program(backend).unwrap_or(false)),"selected backend artifact is absent from this explicit image profile");
        Ok(match backend {HostBackend::Codex=>ManagedTaskPrograms::Codex {program:Path::new(GUEST_MODEL).join("codex").display().to_string()},
            HostBackend::Claude=>ManagedTaskPrograms::Claude {node:NODE.into(),rudder_cli:format!("{GUEST_PACKAGE}/dist/index.js"),claude:format!("{GUEST_MODEL}/claude")}})
    }
    pub(crate) fn settings(&self)->Result<jj_lib::settings::UserSettings> {
        settings_from_config(&self.config)
    }
    pub(crate) fn resources(&self,source:crate::managed_machine_entry::ManagedSeedTaskSource,backend:HostBackend)->Result<ManagedTaskResources> {
        // These owners retain host-imported environments only. Projection DATA
        // cannot replace missing composed preparation/environment custody.
        crate::managed_app::runtime_task::require_legacy_environment_backing(&self.config)?;
        let HostRuntimeProfile::FlyManagedImageV1 {user_name,user_email}=&self.config.data().runtime_profile;
        let mut runtime=vec![ReadOnlyRuntime {source:"/usr".into(),guest_path:"/usr".into()},ReadOnlyRuntime {source:"/opt/rudder".into(),guest_path:GUEST_PACKAGE.into()},ReadOnlyRuntime {source:ROOT.into(),guest_path:GUEST_MODEL.into()}];
        runtime.extend(self.network.as_ref().context("actual declared VM network inputs absent")?.mounts());
        if let Some(fresh)=&self.fresh{for mount in fresh.static_mounts(){if let Some(old)=runtime.iter().find(|r|r.guest_path==mount.guest_path){ensure!(old.source==mount.source,"declared static/runtime mount source differs");}else{runtime.push(mount);}}}
        Ok(ManagedTaskResources {backed_inputs:source.backed_inputs,expected_integration_head:source.expected_integration_head,store:source.store,runnable_root:source.runnable_root,seed_source:source.seed_source,
            seed:source.seed,programs:self.programs(backend)?,runtime,protected_credential_roots:vec![self.project.home.clone(),self.config.data().state_directory.clone()],
            endpoint_parent:self.project.endpoints.clone(),native_program:NATIVE.into(),native_guest:format!("{GUEST_PACKAGE}/dist/native/rudder-native"),jj_guest:format!("{GUEST_PACKAGE}/dist/native/rudder-jj"),
            user_name:user_name.clone(),user_email:user_email.clone(),timeout_seconds:24*60*60,preparation_cache:source.preparation_cache})
    }
}
fn clock<B:RuntimeReadBudget+?Sized>(original:&B,cancelled:&AtomicBool)->Result<()> {if cancelled.load(Ordering::Acquire){original.cancel();anyhow::bail!("original runtime capture cancelled");}original.remaining()?;Ok(())}
fn package_stamp(path:&Path)->Result<Stamp> {let m=fs::symlink_metadata(path)?;if !m.file_type().is_symlink(){return stamp(path);}ensure!(m.uid()==0&&m.gid()==0,"package link is not image owned");let target=path.canonicalize()?;ensure!(fs::read_link(path)?.is_relative()&&target.starts_with("/opt/rudder"),"package symlink escapes relocatable fixed image closure");
    Ok(Stamp {path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec()),mode:m.mode()})}
fn capture_package<B:RuntimeReadBudget+?Sized>(root:&Path,out:&mut Vec<Stamp>,hashes:&mut Vec<(PathBuf,String,u32)>,original:&B,cancelled:&AtomicBool,anchors:&mut DirectoryAnchors)->Result<()> {
    let mut pending=vec![root.to_path_buf()];let mut total=0u64;
    while let Some(path)=pending.pop(){clock(original,cancelled)?;ensure!(out.len()<MAX_FILES,"fixed package closure exceeds entry bound");let before=package_stamp(&path)?;
        match before.mode&libc::S_IFMT {libc::S_IFDIR=>{anchors.capture_expected(&path,&before)?;for item in fs::read_dir(&path)? {pending.push(item?.path());ensure!(pending.len()+out.len()<MAX_FILES,"fixed package enumeration exceeds bound");}},
            libc::S_IFREG=>{total=total.checked_add(before.len).context("package bytes overflow")?;ensure!(total<=MAX_BYTES,"fixed package byte closure exceeds bound");
                let digest=ImageFile::open_anchored(&path,before.len.max(1),original,cancelled,anchors)?.sha;hashes.push((path.clone(),digest,before.mode));},
            libc::S_IFLNK=>{hashes.push((path.clone(),format!("{:x}",Sha256::digest(fs::read_link(&path)?.as_os_str().as_encoded_bytes())),before.mode));},_=>anyhow::bail!("fixed package contains unsupported device/socket")};ensure!(package_stamp(&path)?==before,"package changed during capture");out.push(before);
    }out.sort_by(|a,b|a.path.cmp(&b.path));hashes.sort_by(|a,b|a.0.cmp(&b.0));Ok(())
}

// Declared amd64 Debian image loader components, supplied inside the existing
// /usr read-only mount. This is not a dependency scanner or a claim that a host
// Codex run qualifies all guest dynamic loading. The final actual guest fixture
// must qualify that fixed OS image. Unsupported layout/architecture fails shut.
struct OsLoaderInputs {files:Vec<ImageFile>,aliases:Vec<(PathBuf,Stamp,PathBuf)>,digest:String}
fn alias_stamp(path:&Path)->Result<Stamp> {let m=fs::symlink_metadata(path)?;ensure!(m.uid()==0&&m.gid()==0&&(m.file_type().is_symlink()||m.is_file()),"declared image alias is not protected");
    if !m.file_type().is_symlink(){ensure!(m.mode()&0o022==0,"declared image alias is writable");}
    Ok(Stamp{path:path.into(),dev:m.dev(),ino:m.ino(),len:m.len(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec()),mode:m.mode()})}
impl OsLoaderInputs {
    fn capture<B:RuntimeReadBudget+?Sized>(original:&B,cancelled:&AtomicBool)->Result<Self> {Self::capture_anchored(original,cancelled,&mut DirectoryAnchors::default())}
    fn capture_anchored<B:RuntimeReadBudget+?Sized>(original:&B,cancelled:&AtomicBool,anchors:&mut DirectoryAnchors)->Result<Self> {
        ensure!(std::env::consts::ARCH=="x86_64","managed image loader architecture unsupported");let mut files=Vec::new();let mut aliases=Vec::new();
        for name in ["/usr/lib64/ld-linux-x86-64.so.2","/usr/lib/x86_64-linux-gnu/libc.so.6","/usr/lib/x86_64-linux-gnu/libm.so.6","/usr/lib/x86_64-linux-gnu/libstdc++.so.6","/usr/lib/x86_64-linux-gnu/libgcc_s.so.1"] {
            clock(original,cancelled)?;let path=PathBuf::from(name);let before=alias_stamp(&path)?;let target=path.canonicalize()?;
            ensure!(target.starts_with("/usr"),"declared image loader escapes mounted OS root");files.push(ImageFile::open_anchored(&target,64*1024*1024,original,cancelled,anchors)?);
            ensure!(alias_stamp(&path)?==before&&path.canonicalize()?==target,"declared image loader changed during capture");aliases.push((path,before,target));
        }
        let identities:Vec<_>=files.iter().map(|f|(&f.stamp.path,&f.sha)).collect();let digest=format!("{:x}",Sha256::digest(canonical_json(&identities)?));let owner=Self{files,aliases,digest};owner.current()?;Ok(owner)
    }
    fn current(&self)->Result<()> {for file in &self.files {file.current()?;}for(path,saved,target)in &self.aliases {ensure!(alias_stamp(path)?==*saved&&path.canonicalize()?==*target,"original declared loader alias changed");}Ok(())}
}

// Declared public system inputs for OrdinaryNetworkV1, separate from immutable
// model IMAGE identity. Per-VM resolver/hosts may legitimately differ; this
// original capture fences new work on change, never renews/adopts it silently.
struct VmNetworkInputs {files:Vec<ImageFile>,targets:Vec<ImageFile>,certs:Directory,cert_stamps:Vec<Stamp>,digest:String}
impl VmNetworkInputs {
    fn capture<B:RuntimeReadBudget+?Sized>(original:&B,cancelled:&AtomicBool)->Result<Self> {Self::capture_anchored(original,cancelled,&mut DirectoryAnchors::default())}
    fn capture_anchored<B:RuntimeReadBudget+?Sized>(original:&B,cancelled:&AtomicBool,anchors:&mut DirectoryAnchors)->Result<Self> {
        let mut files=Vec::new();
        for name in ["/etc/nsswitch.conf","/etc/resolv.conf","/etc/hosts"] {files.push(ImageFile::open_anchored(Path::new(name),1024*1024,original,cancelled,anchors)?);}
        if Path::new("/etc/ld.so.cache").try_exists()? {files.push(ImageFile::open_anchored(Path::new("/etc/ld.so.cache"),16*1024*1024,original,cancelled,anchors)?);}
        let certs=Directory::open_anchored(Path::new("/etc/ssl/certs"),anchors)?;let mut cert_stamps=Vec::new();let mut identities=Vec::new();let mut bytes=0u64;let mut targets=Vec::new();let mut target_names=BTreeSet::new();
        for item in fs::read_dir("/etc/ssl/certs")? {clock(original,cancelled)?;ensure!(cert_stamps.len()<4096,"public CA closure exceeds bound");let path=item?.path();let m=fs::symlink_metadata(&path)?;
            let s=if m.file_type().is_symlink() {let target=path.canonicalize()?;ensure!(m.uid()==0&&m.gid()==0&&(target.starts_with("/usr/share/ca-certificates")||target.starts_with("/usr/local/share/ca-certificates")||target.starts_with("/etc/ssl/certs")),"public CA link escapes declared certificate closure");
                if target_names.insert(target.clone()) {let file=ImageFile::open_anchored(&target,1024*1024,original,cancelled,anchors)?;
                    bytes=bytes.checked_add(file.stamp.len).context("CA bytes overflow")?;ensure!(bytes<=16*1024*1024,"public CA target bytes exceed bound");
                    identities.push((target,file.sha.clone()));targets.push(file);}
                Stamp {path:path.clone(),dev:m.dev(),ino:m.ino(),len:m.len(),mtime:(m.mtime(),m.mtime_nsec()),ctime:(m.ctime(),m.ctime_nsec()),mode:m.mode()}}
            else {stamp(&path)?};
            if m.is_file() {bytes=bytes.checked_add(m.len()).context("CA bytes overflow")?;ensure!(bytes<=16*1024*1024,"public CA bytes exceed bound");let file=ImageFile::open_anchored(&path,m.len().max(1),original,cancelled,anchors)?;identities.push((path.clone(),file.sha.clone()));targets.push(file);}
            else {ensure!(m.file_type().is_symlink(),"public CA closure has directory/device");identities.push((path.clone(),format!("{:x}",Sha256::digest(fs::read_link(&path)?.as_os_str().as_encoded_bytes()))));}
            cert_stamps.push(s);
        }
        identities.sort_by(|a,b|a.0.cmp(&b.0));let system:Vec<_>=files.iter().map(|f|(&f.stamp.path,&f.sha)).collect();let digest=format!("{:x}",Sha256::digest(canonical_json(&(system,identities))?));
        let out=Self {files,targets,certs,cert_stamps,digest};out.current()?;Ok(out)
    }
    fn current(&self)->Result<()> {self.certs.current()?;for f in self.files.iter().chain(&self.targets) {f.current()?;}for s in &self.cert_stamps {
        let m=fs::symlink_metadata(&s.path)?;ensure!(m.uid()==0&&m.gid()==0,"public CA ownership changed");ensure!((m.dev(),m.ino(),m.len(),m.mode(),m.ctime(),m.ctime_nsec(),m.mtime(),m.mtime_nsec())==(s.dev,s.ino,s.len,s.mode,s.ctime.0,s.ctime.1,s.mtime.0,s.mtime.1),"original public CA closure changed");}Ok(())}
    fn mounts(&self)->Vec<ReadOnlyRuntime> {let mut out:Vec<_>=self.files.iter().map(|f|ReadOnlyRuntime {source:f.stamp.path.clone(),guest_path:f.stamp.path.display().to_string()}).collect();
        out.push(ReadOnlyRuntime {source:"/etc/ssl/certs".into(),guest_path:"/etc/ssl/certs".into()});out}
}

#[cfg(test)]
#[path="managed_ingress_runtime_capture_tests.rs"]
mod tests;

fn capture_fixed_image<B:RuntimeReadBudget+?Sized>(config:&Arc<HostIngressConfigOwner>,anchors:&mut DirectoryAnchors,files:&mut Vec<ImageFile>,dirs:&mut Vec<Directory>,package_stamps:&mut Vec<Stamp>,package_hashes:&mut Vec<(PathBuf,String,u32)>,loader:&mut Option<OsLoaderInputs>,original:&B,cancelled:&AtomicBool)->Result<String> {
        let declared=config.data().runtime_manifest.clone();
        dirs.push(Directory::open_anchored(Path::new(ROOT),anchors)?);
        ensure!(dirs[0].stamp.mode&0o222==0,"managed runtime directory must be kernel-mode nonwritable");
        files.push(ImageFile::open_anchored(&Path::new(ROOT).join("manifest.json"),64*1024,original,cancelled,anchors)?);
        ensure!(files[0].stamp.mode&0o7777==0o444&&files[0].sha==declared.manifest_sha256,"actual image manifest differs from configured pin");
        let mut bytes=vec![0;files[0].stamp.len as usize];ensure!(files[0].held.read_at(&mut bytes,0)?==bytes.len(),"actual manifest read incomplete");let actual:Manifest=serde_json::from_slice(&bytes)?;
        ensure!(actual.version==1&&actual.profile_id=="fly-managed-image-v1"&&actual.profile_id==declared.profile_id&&actual.entries==declared.entries,"actual manifest/declared closure differs");
        files[0].current()?;
        let mut actual_names=BTreeSet::new();let mut pending=vec![PathBuf::from(ROOT)];let expected:BTreeSet<_>=actual.entries.iter().map(|e|e.path.clone()).collect();let mut count=0usize;
        while let Some(directory)=pending.pop() {clock(original,cancelled)?;
            for item in fs::read_dir(directory)? {let item=item?;count+=1;ensure!(count<=128,"managed image closure exceeds bound");let path=item.path();let s=stamp(&path)?;
                if s.mode&libc::S_IFMT==libc::S_IFDIR {ensure!(s.mode&0o222==0,"managed runtime child directory writable");dirs.push(Directory::open_anchored(&path,anchors)?);pending.push(path);}
                else {ensure!(s.mode&libc::S_IFMT==libc::S_IFREG,"managed runtime has unsupported link/device");if path!=Path::new(ROOT).join("manifest.json"){actual_names.insert(path);}}
            }
        }
        ensure!(actual_names==expected,"managed runtime closure has missing/undeclared files");let mut total=0u64;
        for entry in actual.entries {total=total.checked_add(entry.size_bytes).context("runtime bytes overflow")?;ensure!(total<=MAX_BYTES,"declared runtime bytes exceed bound");
            let file=ImageFile::open_anchored(&entry.path,entry.size_bytes,original,cancelled,anchors)?;ensure!(file.stamp.len==entry.size_bytes&&file.sha==entry.sha256&&file.stamp.mode&0o222==0,"declared actual runtime file differs/writable");files.push(file);}
        for path in [NATIVE,JJ,NODE,CLI,PROFILE] {let file=ImageFile::open_anchored(Path::new(path),512*1024*1024,original,cancelled,anchors)?;ensure!(file.stamp.len>0,"fixed image component empty");files.push(file);}
        // The actual running native image, not another file placed at its path.
        let own=File::open("/proc/self/exe")?.metadata()?;let native=files.iter().find(|f|f.stamp.path==Path::new(NATIVE)).unwrap();
        ensure!((own.dev(),own.ino())==(native.stamp.dev,native.stamp.ino),"guest native differs from original running image");
        dirs.push(Directory::open_anchored(Path::new("/usr"),anchors)?);
        // Provider payload is excluded from IMAGE bytes and hidden by the
        // generic execution root. Retain the actual Root-owned boundary only;
        // this proves no approved kernel ABI or provider module qualification.
        dirs.push(Directory::open_anchored(Path::new(rudder_engine::execution::provider_kernel::PROVIDER_KERNEL_DIRECTORY),anchors)?);
        dirs.push(Directory::open_anchored(Path::new("/opt/rudder"),anchors)?);
        // Capture complete fixed package closure once, including relative
        // image-owned symlinks resolving inside this same package. No volume or
        // updater target can enter its guest mount or host profile helper.
        capture_package(Path::new("/opt/rudder"),package_stamps,package_hashes,original,cancelled,anchors)?;
        *loader=Some(OsLoaderInputs::capture_anchored(original,cancelled,anchors)?);
    let identity:Vec<_>=files.iter().map(|f|(&f.stamp.path,&f.sha,f.stamp.len)).collect();
    Ok(format!("{:x}",Sha256::digest(canonical_json(&(&declared.manifest_sha256,identity,package_hashes,&loader.as_ref().unwrap().digest))?)))
}

#[path="managed_runtime_image_preseed_owner.rs"]
pub(crate) mod image_preseed;

fn settings_from_config(config:&HostIngressConfigOwner)->Result<jj_lib::settings::UserSettings>{
    use jj_lib::config::{StackedConfig,ConfigLayer,ConfigSource};let HostRuntimeProfile::FlyManagedImageV1{user_name,user_email}=&config.data().runtime_profile;
    let mut c=StackedConfig::with_defaults();let mut layer=ConfigLayer::empty(ConfigSource::User);layer.set_value("user.name",user_name.as_str())?;layer.set_value("user.email",user_email.as_str())?;c.add_layer(layer);Ok(jj_lib::settings::UserSettings::from_config(c)?)
}
#[path="managed_fresh_environment_owner.rs"]
pub(crate) mod fresh_environment;

#[path="managed_backed_environment.rs"]
pub(crate) mod backed_environment;

#[path="managed_runtime_environment_owner.rs"]
pub(crate) mod runtime_environment;

#[path="managed_backed_runtime_profile.rs"]mod backed_runtime_profile;
