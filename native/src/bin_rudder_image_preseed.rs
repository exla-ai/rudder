//! Build-time DATA emitter, not a Root/bootstrap/ready API. Install final image
//! files FIRST. Output is outside every captured root to avoid self-reference.
#[cfg(target_os="linux")]
fn main()->anyhow::Result<()> {
    use std::{fs,io::{Read,Write},os::unix::fs::{DirBuilderExt,OpenOptionsExt,MetadataExt,PermissionsExt},path::Path,sync::{Arc,atomic::AtomicBool},time::Duration};
    use rudder_native::{engine_registration::ImagePreseedBudget,image_preseed_data::ImageDataBuild};
    let args=std::env::args_os().skip(1).collect::<Vec<_>>();
    let verify=args.as_slice()==[std::ffi::OsString::from("--verify")];
    let bake=args.as_slice()==[std::ffi::OsString::from("--bake-cas-v1")];
    anyhow::ensure!(args.is_empty()||verify||bake,"image DATA emitter accepts only fixed image profiles, never paths or runtime authority");
    anyhow::ensure!(unsafe{libc::getuid()}==0&&unsafe{libc::geteuid()}==0,"image DATA build must run as actual image Root");
    if bake{return bake_complete_cas();}
    let output=Path::new("/var/lib/rudder-image-preseed-v1");
    if verify {let m=fs::symlink_metadata(output)?;anyhow::ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o7777==0o555,"image DATA verification output is not protected");}
    else {anyhow::ensure!(output.symlink_metadata().err().is_some_and(|e|e.kind()==std::io::ErrorKind::NotFound),"image DATA output already exists; never overwrite");}
    for parent in output.parent().unwrap().ancestors(){let m=fs::symlink_metadata(parent)?;anyhow::ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"image DATA output ancestry is not protected");}
    let original=ImagePreseedBudget::begin(Duration::from_secs(300))?;let cancelled=Arc::new(AtomicBool::new(false));
    let workspace=Path::new("/workspace");
    for parent in workspace.ancestors(){let m=fs::symlink_metadata(parent)?;anyhow::ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"image DATA scratch ancestry is not Root controlled");}
    let scratch=workspace.join(format!("rudder-image-data-{}",uuid::Uuid::new_v4()));fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    let store=rudder_engine::store::LocalStore::open(scratch.join("cas"))?;let mut build=None;
    // No Runtime/Root/task capability is constructed by these verified bytes.
    ImageDataBuild::capture_retaining(&mut build,store.clone(),original.clone(),cancelled)?;let data=build.as_ref().unwrap();data.check_deep_for_import()?;let pin=data.pin_data()?;
    if !verify {fs::DirBuilder::new().mode(0o700).create(output)?;}
    for(name,bytes)in [("manifest.json",data.manifest_bytes()?.to_vec()),("pin.json",rudder_engine::protocol::canonical_json(&pin)?)] {
        original.remaining()?;
        if verify {
            let file=fs::OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(output.join(name))?;let m=file.metadata()?;
            anyhow::ensure!(m.is_file()&&m.uid()==0&&m.gid()==0&&m.mode()&0o7777==0o444&&m.nlink()==1&&m.len()==bytes.len()as u64,"image DATA verification file identity differs: {} (regular={}, uid={}, gid={}, mode={:o}, links={}, installed_bytes={}, rebuilt_bytes={})",name,m.is_file(),m.uid(),m.gid(),m.mode()&0o7777,m.nlink(),m.len(),bytes.len());
            let mut actual=Vec::new();std::io::Read::take(file,bytes.len()as u64+1).read_to_end(&mut actual)?;
            anyhow::ensure!(actual==bytes,"installed image tree differs from the baked DATA manifest/pin");
        } else {let mut file=fs::OpenOptions::new().write(true).create_new(true).mode(0o444).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(output.join(name))?;file.write_all(&bytes)?;file.sync_all()?;}
    }
    data.check_deep_for_import()?;original.remaining()?;
    if !verify {fs::set_permissions(output,fs::Permissions::from_mode(0o555))?;fs::File::open(output)?.sync_all()?;fs::File::open(output.parent().unwrap())?.sync_all()?;}
    for name in ["manifest.json","pin.json"]{let m=fs::symlink_metadata(output.join(name))?;anyhow::ensure!(m.is_file()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o7777==0o444&&m.nlink()==1,"image DATA output mode/owner invalid");}
    if !verify {
        let objects=data.objects_for_import()?;
        let bytes=objects.iter().try_fold(0u64,|total,object|total.checked_add(object.size_bytes).ok_or_else(||anyhow::anyhow!("image graph size overflow")))?;
        println!("RUDDER_IMAGE_PRESEED_OBJECT_GRAPH {} {}",objects.len(),bytes);
    }
    // Own build scratch only; no source/history/user store can be removed.
    drop(build);drop(store);fs::remove_dir_all(&scratch)?;
    if verify {println!("RUDDER_IMAGE_TREE_VERIFIED {} {}",pin.manifest.digest,pin.manifest.size_bytes);}
    Ok(())
}

/// Separate build-only image profile. Writes directly into its fresh final
/// DATA directory, inaccessible at0700 until all actual IO/ACKs settle. No
/// second payload copy, existing destination adoption or cleanup on error.
#[cfg(target_os="linux")]
fn bake_complete_cas()->anyhow::Result<()> {
    use anyhow::{ensure,Context};
    use std::{collections::{BTreeMap,BTreeSet},fs::{self,File,OpenOptions},io::Write,
        os::{fd::OwnedFd,unix::fs::{DirBuilderExt,FileExt,MetadataExt,OpenOptionsExt,PermissionsExt}},
        path::Path,sync::{Arc,atomic::AtomicBool},time::Duration};
    use rudder_engine::{protocol::{canonical_json,ObjectKind,ObjectRef},store::{LocalStore,ImageImportFence,
        baked_image::{BakedImageCas,BakedImageInventoryData,BakedImageRoots,BAKED_IMAGE_DIRECTORY,BAKED_IMAGE_LAYOUT,MAX_BAKED_INVENTORY_BYTES}}};
    use rudder_native::{engine_registration::ImagePreseedBudget,image_preseed_data::ImageDataBuild};
    struct Clock(ImagePreseedBudget);
    impl ImageImportFence for Clock{fn remaining(&self)->anyhow::Result<Duration>{self.0.remaining()}}
    // Actual partial carriers exist before directory/CAS creation and remain
    // local through every synchronous call. No Err removes the partial output.
    struct Owned {store:Option<LocalStore>,build:Option<ImageDataBuild>,io:Option<File>,dirs:Vec<File>,reader:Option<Arc<BakedImageCas>>}
    let mut owned=Owned{store:None,build:None,io:None,dirs:Vec::new(),reader:None};
    ensure!(unsafe{libc::getuid()}==0&&unsafe{libc::geteuid()}==0&&unsafe{libc::getgid()}==0&&unsafe{libc::getegid()}==0,"baked image profile requires Root0/gid0");
    let original=ImagePreseedBudget::begin(Duration::from_secs(300))?;let output=Path::new(BAKED_IMAGE_DIRECTORY);
    for parent in output.parent().unwrap().ancestors(){original.remaining()?;let m=fs::symlink_metadata(parent)?;
        ensure!(m.is_dir()&&!m.file_type().is_symlink()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"baked DATA ancestry not Root protected");}
    ensure!(output.symlink_metadata().err().is_some_and(|e|e.kind()==std::io::ErrorKind::NotFound),"baked image output exists; original recovery required");
    original.remaining()?;fs::DirBuilder::new().mode(0o700).create(output)?;
    owned.dirs.push(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(output)?);
    original.remaining()?;owned.store=Some(LocalStore::open(output.join("cas"))?);
    ImageDataBuild::capture_retaining(&mut owned.build,owned.store.as_ref().unwrap().clone(),original.clone(),Arc::new(AtomicBool::new(false)))?;
    let data=owned.build.as_ref().unwrap();data.check_deep_for_import()?;
    let pin=data.pin_data()?;let objects=data.objects_for_import()?;
    // These getters require the SAME successful LocalImageImport two-pin ACK
    // and known normal lock/IO release. Inventory is never an ACK substitute.
    original.remaining()?;let retained=owned.store.as_ref().unwrap().retention()?;
    ensure!(retained.format_version==1&&retained.pins.len()==2&&retained.staging.is_empty()&&retained.image_import_holds.is_empty()
        &&retained.pins.values().any(|r|r==&pin.manifest)&&retained.pins.values().any(|r|r==&pin.installer_input),"original baked image pin ACK differs");
    owned.io=Some(OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(output.join("cas/retention.json"))?);
    let before=owned.io.as_ref().unwrap().metadata()?;
    ensure!(before.is_file()&&before.uid()==0&&before.gid()==0&&before.nlink()==1&&before.len()<=MAX_BAKED_INVENTORY_BYTES,"baked retention snapshot invalid");
    let mut raw=vec![0;usize::try_from(before.len())?];let mut offset=0usize;
    while offset<raw.len(){original.remaining()?;let end=(offset+64*1024).min(raw.len());let n=owned.io.as_ref().unwrap().read_at(&mut raw[offset..end],offset as u64)?;
        ensure!(n>0,"baked retention snapshot ended early");offset+=n;}
    let value:serde_json::Value=serde_json::from_slice(&raw)?;
    ensure!(serde_json::from_value::<rudder_engine::store::RetentionState>(value["state"].clone())?==retained,"baked original retention snapshot changed");
    let after=owned.io.as_ref().unwrap().metadata()?;
    ensure!((before.dev(),before.ino(),before.len(),before.ctime(),before.ctime_nsec())==(after.dev(),after.ino(),after.len(),after.ctime(),after.ctime_nsec()),"baked retention inode changed");
    owned.io.take();original.remaining()?;
    let total=objects.iter().try_fold(0u64,|n,r|n.checked_add(r.size_bytes).context("baked graph size overflow"))?;
    let roots=BakedImageRoots{format_version:1,manifest:pin.manifest.clone(),installer_input:pin.installer_input.clone()};
    let inventory=BakedImageInventoryData{format_version:1,layout:BAKED_IMAGE_LAYOUT.into(),roots:roots.clone(),objects,total_bytes:total,
        retention:ObjectRef::from_bytes(ObjectKind::Blob,&raw),retention_generation:retained.generation,pins:retained.pins};
    inventory.validate()?;let inventory_bytes=canonical_json(&inventory)?;
    ensure!(inventory_bytes.len()as u64<=MAX_BAKED_INVENTORY_BYTES,"baked inventory DATA exceeds bound");
    let inventory_ref=ObjectRef::from_bytes(ObjectKind::Blob,&inventory_bytes);
    for(name,bytes)in [("manifest.json",data.manifest_bytes()?.to_vec()),("pin.json",canonical_json(&roots)?),("inventory.json",inventory_bytes)]{
        original.remaining()?;owned.io=Some(OpenOptions::new().write(true).create_new(true).mode(0o444).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(output.join(name))?);
        for chunk in bytes.chunks(64*1024){original.remaining()?;owned.io.as_mut().unwrap().write_all(chunk)?;}
        owned.io.as_ref().unwrap().sync_all()?;original.remaining()?;owned.io.take();
    }
    data.check_deep_for_import()?;data.objects_for_import()?;original.remaining()?;
    // Validate/seal only the actual fresh CAS. Never chmod a user/existing CAS.
    // No payload is recopied or rehashed here: original import already verified
    // and durably committed every complete graph object before this point.
    let mut expected=BTreeMap::new();for reference in &inventory.objects {
        if let Some(size)=expected.insert(reference.digest.clone(),reference.size_bytes){ensure!(size==reference.size_bytes,"same raw hash has different size");}}
    let mut found=BTreeSet::new();let cas=output.join("cas");
    ensure!(fs::read_dir(&cas)?.map(|e|e.map(|e|e.file_name().to_string_lossy().into_owned())).collect::<std::io::Result<BTreeSet<_>>>()?
        ==BTreeSet::from(["lock".into(),"objects".into(),"retention.json".into(),"tmp".into()]),"unexpected baked CAS control entry");
    for shard in fs::read_dir(cas.join("objects"))?{
        original.remaining()?;let shard=shard?;let name=shard.file_name().into_string().map_err(|_|anyhow::anyhow!("invalid baked shard"))?;
        ensure!(name.len()==2&&name.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase()),"invalid baked shard name");
        ensure!(owned.dirs.len()<260,"baked directory FD bound");
        owned.dirs.push(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(shard.path())?);
        for object in fs::read_dir(shard.path())?{
            original.remaining()?;let object=object?;let suffix=object.file_name().into_string().map_err(|_|anyhow::anyhow!("invalid baked object"))?;
            ensure!(suffix.len()==62&&suffix.bytes().all(|b|b.is_ascii_hexdigit()&&!b.is_ascii_uppercase()),"invalid baked object name");
            let hash=name.clone()+&suffix;let size=*expected.get(&hash).context("unreachable/unannounced baked CAS object; original recovery required")?;
            ensure!(found.insert(hash),"duplicate baked object path");
            owned.io=Some(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_CLOEXEC|libc::O_NONBLOCK).open(object.path())?);
            let m=owned.io.as_ref().unwrap().metadata()?;ensure!(m.is_file()&&m.uid()==0&&m.gid()==0&&m.nlink()==1&&m.mode()&0o7777==0o444&&m.len()==size,"baked object inode differs");
            original.remaining()?;owned.io.take();
        }
    }
    ensure!(found.len()==expected.len(),"complete baked CAS has missing payloads");
    ensure!(fs::read_dir(cas.join("tmp"))?.next().is_none(),"original baked CAS still has unsettled temporary IO");
    for name in ["lock","retention.json"]{
        original.remaining()?;owned.io=Some(OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW|libc::O_NONBLOCK).open(cas.join(name))?);
        let m=owned.io.as_ref().unwrap().metadata()?;ensure!(m.is_file()&&m.uid()==0&&m.gid()==0&&m.nlink()==1&&m.mode()&0o077==0,"baked control inode differs");
        owned.io.as_ref().unwrap().set_permissions(fs::Permissions::from_mode(0o444))?;owned.io.as_ref().unwrap().sync_all()?;original.remaining()?;owned.io.take();
    }
    for path in [cas.join("objects"),cas.join("tmp"),cas.clone()]{
        original.remaining()?;owned.dirs.push(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(path)?);
    }
    for file in owned.dirs.iter().skip(1){original.remaining()?;let m=file.metadata()?;
        ensure!(m.is_dir()&&m.uid()==0&&m.gid()==0&&m.mode()&0o022==0,"baked directory is not original Root custody");
        file.set_permissions(fs::Permissions::from_mode(0o555))?;file.sync_all()?;original.remaining()?;}
    // Final marker/reader follows known IO settlement and all original checks.
    owned.dirs[0].set_permissions(fs::Permissions::from_mode(0o555))?;owned.dirs[0].sync_all()?;
    owned.dirs.push(OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW).open(output.parent().unwrap())?);
    owned.dirs.last().unwrap().sync_all()?;original.remaining()?;
    let root:OwnedFd=owned.dirs[0].try_clone()?.into();
    let inventory_fd:OwnedFd=OpenOptions::new().read(true).custom_flags(libc::O_CLOEXEC|libc::O_NOFOLLOW).open(output.join("inventory.json"))?.into();
    BakedImageCas::capture_retaining(&mut owned.reader,root,inventory_fd,inventory_ref.clone(),Arc::new(Clock(original.clone())))?;
    original.remaining()?;
    println!("RUDDER_BAKED_IMAGE_CAS_DATA {}",serde_json::json!({"format_version":1,"layout":BAKED_IMAGE_LAYOUT,
        "directory":BAKED_IMAGE_DIRECTORY,"inventory":inventory_ref,"roots":roots,"objects":inventory.objects.len(),"total_bytes":inventory.total_bytes,
        "retention_generation":inventory.retention_generation,"scope":"immutable-image-cas-data-only","production_admission":false,"runtime_mount":false}));
    Ok(())
}
#[cfg(not(target_os="linux"))]
fn main(){eprintln!("fixed image DATA emitter requires Linux");std::process::exit(78);}
