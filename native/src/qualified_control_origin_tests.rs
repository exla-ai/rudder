//! DATA and original-file mechanisms only. Written UNRUN; no source/Root or
//! engine authority can be reconstructed from these fixtures.
use super::*;

fn config()->HostIngressConfig {
    let home=PathBuf::from("/workspace/origin-fixture/home");let checkout=PathBuf::from("/workspace/origin-fixture/project");
    let namespace=rudder_engine::journal::ManagedProjectNamespace {format_version:1,project_id:"project".into(),home:home.clone(),
        journal:home.join("fabric/journal.sqlite"),repository:checkout.clone(),directory:rudder_engine::journal::ManagedProjectNamespace::directory_for(&home,"project")};
    HostIngressConfig {seed_profile:None,version:1,machine_id:"machine".into(),store_id:"store".into(),incarnation:"incarnation".into(),account_id:"account".into(),home_id:"home".into(),cloud_origin:PRODUCTION_CONTROL_ORIGIN.into(),state_directory:"/workspace/origin-fixture/state".into(),store_policy:HostReceiptStorePolicy::InitializeEmptyV1,
        projects:vec![AllowedPreparedProject {alias:"project".into(),checkout,home,cas:"/workspace/origin-fixture/cas".into(),endpoints:"/workspace/origin-fixture/endpoints".into(),prepare:ManagedProjectPrepare {request_id:uuid::Uuid::new_v4().to_string(),namespace}}],
        runtime_profile:HostRuntimeProfile::FlyManagedImageV1 {user_name:"Fixture".into(),user_email:"fixture@example.invalid".into()},runtime_manifest:ImageRuntimeManifest {profile_id:"fly-managed-image-v1".into(),manifest_sha256:"a".repeat(64),entries:vec![ImageRuntimeEntry {path:"/opt/rudder-managed-runtime/v1/codex".into(),sha256:"b".repeat(64),size_bytes:1}]},image_preseed:None,owner_lease_safety:None}
}
fn scope()->UploadScope {UploadScope {version:1,home_id:"home".into(),workspace_id:"project".into(),transfer_id:uuid::Uuid::new_v4().to_string(),request_nonce:uuid::Uuid::new_v4().to_string(),mode:"fresh".into(),maximum_objects:128,maximum_bytes:4194304,maximum_object_bytes:1048576,constraint:FinalizationBudget::begin(Duration::from_secs(30)).unwrap().constraint().into(),manifest:None,maximum_manifest_bytes:None}}

#[test]
fn finite_control_origins_are_exact_without_url_normalization() {
    for value in [PRODUCTION_CONTROL_ORIGIN,QUALIFICATION_CONTROL_ORIGIN] {assert_eq!(cloud_control_origin(value).unwrap(),value);let mut c=config();c.cloud_origin=value.into();c.validate().unwrap();}
    for value in ["https://rudder-v4-qualification.fly.dev/","https://rudder-v4-qualification.fly.dev:443","https://rudder-v4-qualification.fly.dev/path","https://rudder-v4-qualification.fly.dev?x=1","https://u:p@rudder-v4-qualification.fly.dev","http://rudder-v4-qualification.fly.dev","wss://rudder-v4-qualification.fly.dev","https://rudder-v4-qualification.fly.dev.evil.invalid","https://rudder.exla.ai"] {assert!(cloud_control_origin(value).is_err());let mut c=config();c.cloud_origin=value.into();assert!(c.validate().is_err());}
}
#[test]
fn uploader_binds_actual_config_data_to_original_home_and_project() {
    let s=scope();s.validate().unwrap();let mut c=config();validate_upload_config(&c,&s).unwrap();c.cloud_origin=QUALIFICATION_CONTROL_ORIGIN.into();validate_upload_config(&c,&s).unwrap();
    c.home_id="foreign".into();assert!(validate_upload_config(&c,&s).is_err());c.home_id=s.home_id.clone();c.projects[0].alias="foreign".into();assert!(validate_upload_config(&c,&s).is_err());
}
#[test]
fn config_origin_changes_original_raw_hash_without_extending_upload_wire() {
    let a=config();let mut b=a.clone();b.cloud_origin=QUALIFICATION_CONTROL_ORIGIN.into();assert_ne!(Sha256::digest(serde_json::to_vec(&a).unwrap()),Sha256::digest(serde_json::to_vec(&b).unwrap()));
    let s=scope();let mut raw=serde_json::to_value(&s).unwrap();raw["cloud_origin"]=serde_json::json!(QUALIFICATION_CONTROL_ORIGIN);assert!(serde_json::from_value::<UploadScope>(raw).is_err());
    let raw=serde_json::to_value(&s).unwrap();assert!(raw.get("token").is_none());assert!(raw.get("config").is_none());assert!(raw.get("cloud_origin").is_none());
}

#[test]
#[ignore="requires explicit disposable ROOT process; original file mechanisms only, no provider or activation"]
fn original_config_fd7_duplicate_is_readonly_and_replacement_is_refused()->Result<()> {
    use std::{io::Write,os::unix::fs::DirBuilderExt};
    ensure!(unsafe {libc::getuid()}==0&&unsafe {libc::getgid()}==0,"root-only original FD fixture");
    let dir=PathBuf::from(format!("/run/rudder-origin-fd7-{}",uuid::Uuid::new_v4()));std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let path=dir.join("config.json");let bytes=serde_json::to_vec(&config())?;
    let mut writer=OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;writer.write_all(&bytes)?;writer.sync_all()?;
    let (file,stamp)=open_root(&path,0o600)?;let original=ImmutableFile {file,stamp:stamp.clone(),path:path.clone(),mode:0o600};let digest=format!("{:x}",Sha256::digest(&bytes));
    let duplicate=duplicate_original_config(&original,&digest)?;let dup_file=File::from(duplicate);stamp.check(&dup_file,0o600)?;
    let flags=unsafe {libc::fcntl(dup_file.as_raw_fd(),libc::F_GETFL)};ensure!(flags>=0&&flags&libc::O_ACCMODE==libc::O_RDONLY,"duplicate became writable");
    let writable=ImmutableFile {file:writer.try_clone()?,stamp:stamp.clone(),path:path.clone(),mode:0o600};ensure!(duplicate_original_config(&writable,&digest).is_err(),"writable config descriptor accepted");
    let held_path=dir.join("held-original");fs::rename(&path,&held_path)?;let mut replacement=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;replacement.write_all(&bytes)?;replacement.sync_all()?;
    ensure!(duplicate_original_config(&original,&digest).is_err(),"same-bytes replacement accepted");ensure!(read_at(&dup_file,MAX_HOST_FRAME)?==bytes,"duplicate switched original inode");
    drop(replacement);drop(writable);drop(writer);drop(original);drop(dup_file);fs::remove_file(path)?;fs::remove_file(held_path)?;fs::remove_dir(dir)?;Ok(())
}
