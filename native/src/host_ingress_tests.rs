//! DATA validation regressions only; no reconstructed live host authority.
use super::*;
fn command() -> HostCommand {HostCommand {version:1,session_generation:uuid::Uuid::new_v4().to_string(),rpc_id:"rpc-1".into(),
    command_key:"stable-1".into(),operation:HostOperation::StartTaskV1 {project_alias:"prepared".into(),backend:HostBackend::Codex,
    model:"explicit-model".into(),profile_id:"explicit-api-profile".into(),text:"edit first\nthen verify".into()},
    constraint:FinalizationBudget::begin(Duration::from_secs(20)).unwrap().constraint()}}
fn config()->HostIngressConfig {
    let home=PathBuf::from("/workspace/private/home");let checkout=PathBuf::from("/workspace/prepared/project");
    let namespace=rudder_engine::journal::ManagedProjectNamespace {format_version:1,project_id:"project-1".into(),home:home.clone(),
        journal:home.join("fabric/journal.sqlite"),repository:checkout.clone(),
        directory:rudder_engine::journal::ManagedProjectNamespace::directory_for(&home,"project-1")};
    HostIngressConfig {sdk_initial_source:None,compiler_sdk:None,compiler_qualification:None,runtime_qualification:None,seed_profile:None,owner_mode:None,owner_lease_safety:None,image_preseed:None,version:1,machine_id:"machine-1".into(),store_id:"store-1".into(),incarnation:"incarnation-1".into(),account_id:"account-1".into(),
        home_id:"home-1".into(),cloud_origin:"https://rudder-cloud-control.fly.dev".into(),state_directory:"/workspace/private/receipt-store".into(),
        store_policy:HostReceiptStorePolicy::InitializeEmptyV1,projects:vec![AllowedPreparedProject {alias:"prepared".into(),checkout,home,
            cas:"/workspace/private/cas".into(),endpoints:"/workspace/private/endpoints".into(),
            prepare:ManagedProjectPrepare {request_id:uuid::Uuid::new_v4().to_string(),namespace}}],
        runtime_profile:HostRuntimeProfile::FlyManagedImageV1 {user_name:"Test User".into(),user_email:"test@example.invalid".into()},
        runtime_manifest:ImageRuntimeManifest {profile_id:"fly-managed-image-v1".into(),manifest_sha256:"a".repeat(64),
            entries:vec![ImageRuntimeEntry {path:"/opt/rudder-managed-runtime/v1/codex".into(),sha256:"b".repeat(64),size_bytes:123}]}}
}
#[test]
fn host_data_is_not_authority_and_real_owner_is_send_sync() {
    fn send_sync<T:Send+Sync>() {}send_sync::<HostIngressConfigOwner>();send_sync::<HostIngress>();
    config().validate().unwrap();command().validate().unwrap();
}
#[test]
fn host_task_keeps_multiline_text_but_bounds_nul_and_bytes() {
    let mut cmd=command();cmd.validate().unwrap();
    if let HostOperation::StartTaskV1 {text,..}=&mut cmd.operation {*text="x\0y".into();}assert!(cmd.validate().is_err());
    if let HostOperation::StartTaskV1 {text,..}=&mut cmd.operation {*text="x".repeat(24*1024+1);}assert!(cmd.validate().is_err());
}
#[test]
fn host_original_constraint_never_accepts_fresh_thirty_second_policy() {
    let mut cmd=command();cmd.constraint=FinalizationBudget::begin(Duration::from_secs(30)).unwrap().constraint();assert!(cmd.validate().is_err());
}
#[test]
fn host_stop_requires_exact_original_identity_and_a_distinct_command_key() {
    let mut cmd=command();cmd.operation=HostOperation::StopTaskV1 {project_alias:"prepared".into(),lookup_command_key:"original-start".into(),
        command_id:uuid::Uuid::new_v4().to_string(),run_id:"original-run".into(),run_epoch:"7".into(),attempt_id:uuid::Uuid::new_v4().to_string()};
    cmd.validate().unwrap();
    for epoch in ["0","07","+7","-7","18446744073709551616"] {
        let mut bad=cmd.clone();if let HostOperation::StopTaskV1 {run_epoch,..}=&mut bad.operation {*run_epoch=epoch.into();}
        assert!(bad.validate().is_err());
    }
    let mut bad=cmd.clone();if let HostOperation::StopTaskV1 {lookup_command_key,..}=&mut bad.operation {*lookup_command_key=bad.command_key.clone();}
    assert!(bad.validate().is_err());
    let value=serde_json::to_value(&cmd).unwrap();
    for field in ["project_alias","lookup_command_key","command_id","run_id","run_epoch","attempt_id"] {
        let mut missing=value.clone();missing["operation"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<HostCommand>(missing).is_err());
    }
    let mut unknown=value.clone();unknown["operation"]["force"]=true.into();assert!(serde_json::from_value::<HostCommand>(unknown).is_err());
    let mut numeric=value;numeric["operation"]["run_epoch"]=7.into();assert!(serde_json::from_value::<HostCommand>(numeric).is_err());
}
#[test]
fn host_original_prepare_namespace_and_explicit_path_are_bound() {
    let mut c=config();c.projects[0].checkout="/workspace/foreign".into();assert!(c.validate().is_err());
    let mut c=config();c.projects[0].home=PathBuf::from("/workspace/private/home ");assert!(c.validate().is_err());
    let mut c=config();c.projects.push(c.projects[0].clone());assert!(c.validate().is_err());
}
#[test]
fn host_manifest_cannot_select_legacy_volume_runtime() {
    let mut c=config();c.runtime_manifest.entries[0].path="/home/rudder/.local/bin/codex".into();assert!(c.validate().is_err());
    let mut c=config();c.cloud_origin="https://other.example.invalid".into();assert!(c.validate().is_err());
    let mut c=config();c.cloud_origin="https://rudder.exla.ai".into();assert!(c.validate().is_err());
}
#[test]
fn host_reply_requires_all_original_transport_bindings_and_one_outcome() {
    let cmd=command();let mut reply=HostReply {version:1,session_generation:cmd.session_generation.clone(),rpc_id:cmd.rpc_id.clone(),
        command_key:cmd.command_key.clone(),result:Some(serde_json::json!({"receipt":"data-only"})),error:None};
    reply.validate_for(&cmd).unwrap();reply.session_generation=uuid::Uuid::new_v4().to_string();assert!(reply.validate_for(&cmd).is_err());
    reply.session_generation=cmd.session_generation.clone();reply.command_key="other".into();assert!(reply.validate_for(&cmd).is_err());
    reply.command_key=cmd.command_key.clone();reply.result=Some(serde_json::Value::Null);assert!(reply.validate_for(&cmd).is_err());
    reply.result=Some(serde_json::json!({"receipt":"data-only"}));reply.error=Some(HostReplyError::EffectUnknown);assert!(reply.validate_for(&cmd).is_err());
}
#[test]
fn host_stable_operation_hash_excludes_new_transport_identity() {
    let a=command();let mut b=a.clone();b.session_generation=uuid::Uuid::new_v4().to_string();b.rpc_id="new-rpc".into();
    b.constraint=FinalizationBudget::begin(Duration::from_secs(10)).unwrap().constraint();
    assert_eq!(canonical_json(&a.operation).unwrap(),canonical_json(&b.operation).unwrap());
    assert_ne!(canonical_json(&a).unwrap(),canonical_json(&b).unwrap());
    // The actual durable native store owns deduplication; no transport replay.
}

#[test]
fn paged_seed_policy_is_explicit_and_absence_preserves_v1(){
    let mut bytes=serde_json::to_value(config()).unwrap();bytes.as_object_mut().unwrap().remove("seed_profile");
    let old:HostIngressConfig=serde_json::from_value(bytes).unwrap();assert_eq!(old.seed_profile.unwrap_or_default(),rudder_vcs_bridge::seed::SeedProfile::SmallV1);
    let mut selected=old;selected.seed_profile=Some(rudder_vcs_bridge::seed::SeedProfile::PagedCowV2);
    let selected:HostIngressConfig=serde_json::from_slice(&canonical_json(&selected).unwrap()).unwrap();
    assert_eq!(selected.seed_profile,Some(rudder_vcs_bridge::seed::SeedProfile::PagedCowV2));
    // Config DATA only; no Root/config/seed/readiness owner was constructed.
}


// Restriction DATA only: these fixtures construct no protected config owner,
// registered engine, ready observer, provider or successful qualification ACK.
fn qualification_scope_data_for_test()->super::super::runtime_image::qualification::RuntimeQualificationScope {
    use rudder_engine::protocol::{ObjectRef,ObjectKind};
    super::super::runtime_image::qualification::RuntimeQualificationScope {
        format_version:1,scope:"disposable-runtime-floor-and-original-client-only".into(),
        qualification_id:uuid::Uuid::new_v4().to_string(),qualification_nonce:uuid::Uuid::new_v4().to_string(),
        image:format!("registry.fly.io/rudder-workers-exla@sha256:{}","a".repeat(64)),
        image_receipt_sha256:"b".repeat(64),source_manifest_sha256:"c".repeat(64),
        inventory:ObjectRef::from_bytes(ObjectKind::Blob,b"inventory DATA"),
        manifest:ObjectRef::from_bytes(ObjectKind::Blob,b"manifest DATA"),
        installer_input:ObjectRef::from_bytes(ObjectKind::Directory,b"directory reference DATA"),
        native_sha256:"d".repeat(64),buddy_sha256:"e".repeat(64),service_sha256:"f".repeat(64),launcher_sha256:"1".repeat(64),
    }
}
#[test]
fn qualification_absence_preserves_config_bytes_and_does_not_create_scope() {
    let original=config();let raw=canonical_json(&original).unwrap();
    let value:serde_json::Value=serde_json::from_slice(&raw).unwrap();assert!(value.get("runtime_qualification").is_none());
    let decoded:HostIngressConfig=serde_json::from_slice(&raw).unwrap();assert!(decoded.runtime_qualification.is_none());
    assert_eq!(canonical_json(&decoded).unwrap(),raw);assert!(decoded.assert_unscoped_operations().is_ok());
}
#[test]
fn qualification_scope_independently_refuses_every_host_and_general_operation() {
    let mut data=config();data.runtime_qualification=Some(qualification_scope_data_for_test());
    assert!(data.assert_unscoped_operations().is_err());
}
#[test]
fn qualification_scope_requires_exact_protected_image_roots_and_nonces() {
    let mut data=config();let scope=qualification_scope_data_for_test();
    assert!(scope.validate_config(&data).is_err());
    data.image_preseed=Some(HostImagePreseedPolicy{image:scope.image.clone(),image_receipt_sha256:scope.image_receipt_sha256.clone(),
        full_manifest_sha256:scope.manifest.digest.clone(),batch_transport:None,
        runtime_mount:Some(HostBakedRuntimePolicy{profile:"baked-cas-v1".into(),inventory:scope.inventory.clone()})});
    scope.validate_config(&data).unwrap();
    let mut changed=scope.clone();changed.qualification_nonce="not an original UUID".into();assert!(changed.validate_config(&data).is_err());
    let mut changed=scope.clone();changed.inventory.digest="0".repeat(64);assert!(changed.validate_config(&data).is_err());
    let mut changed=scope;changed.manifest.size_bytes=32*1024*1024+1;assert!(changed.validate_config(&data).is_err());
}
