//! Actual canonical/HMAC functions against independent synthetic vectors. No
//! live Owner/Root credential/process/PG/source permission is reconstructed.
use super::*;
use std::time::Duration;
const VECTORS:&str=include_str!("../tests/fixtures/engine_registration/hmac-vectors.json");
#[test]
fn root_full_cross_language_registration_and_operation_vectors() {
    let fixture:serde_json::Value=serde_json::from_str(VECTORS).unwrap();assert_eq!(fixture["binding_fields"].as_array().unwrap().len(),21);
    for vector in fixture["vectors"].as_array().unwrap(){let body=&vector["body"];let key=vector["key_hex"].as_str().unwrap().as_bytes().chunks_exact(2).map(|part|unhex::<1>(part).unwrap()[0]).collect::<Vec<_>>();
        let domain=vector["domain"].as_str().unwrap().as_bytes();assert!(domain.ends_with(&[0]));
        assert_eq!(bytes(body).unwrap(),vector["canonical_utf8"].as_str().unwrap().as_bytes());
        assert_eq!(sign(&key,domain,body).unwrap(),vector["mac_hex"].as_str().unwrap());
        assert_eq!(hex(&Sha256::digest(bytes(body).unwrap())),vector["body_sha256"].as_str().unwrap());
        let packet=Signed {body:body.clone(),mac:vector["mac_hex"].as_str().unwrap().into()};verify(&key,domain,&packet).unwrap();
        assert!(verify(&key,if domain==REQUEST_DOMAIN{ACK_DOMAIN}else{REQUEST_DOMAIN},&packet).is_err());
    }
    let proof=fixture["vectors"].as_array().unwrap().iter().find(|v|v["phase"]=="proof").unwrap();
    let parsed:ProofBody=serde_json::from_value(proof["body"].clone()).unwrap();assert_eq!(parsed.evidence.engine.exe.inode,"9007199254740994");
    assert_eq!(serde_json::to_value(parsed.binding).unwrap().as_object().unwrap().len(),21);
}
#[test]
fn operation_ack_matches_original_nonce_body_registration_and_constraint() {
    let fixture:serde_json::Value=serde_json::from_str(VECTORS).unwrap();let values=fixture["vectors"].as_array().unwrap();
    let request:OperationBody=serde_json::from_value(values.iter().find(|v|v["phase"]=="operation_request").unwrap()["body"].clone()).unwrap();
    let ack:EngineOperationAck=serde_json::from_value(values.iter().find(|v|v["phase"]=="operation_ack").unwrap()["body"].clone()).unwrap();
    let registered:RegistrationAck=serde_json::from_value(values.iter().find(|v|v["phase"]=="ack").unwrap()["body"].clone()).unwrap();
    matches_signed(&ack,&request,&registered).unwrap();
    for field in 0..5 {let mut changed=ack.clone();match field{0=>changed.request_nonce=uuid::Uuid::new_v4().to_string(),1=>changed.payload_sha256="f".repeat(64),2=>changed.boot_id=uuid::Uuid::new_v4().to_string(),3=>changed.request_sha256="f".repeat(64),_=>changed.epoch="2".into()};assert!(matches_signed(&changed,&request,&registered).is_err());}
}
#[test]
fn operation_cannot_carry_raw_context_retention_claim_or_arbitrary_method() {
    let value=serde_json::json!({"op":"AdmitAnything","workspace_id":"w","Context":{"trusted":true},"retained":true});
    assert!(serde_json::from_value::<EngineOperationKind>(value).is_err());
    let value=serde_json::json!({"op":"PrepareFreshProjectV1","workspace_id":"w","retained":true});assert!(serde_json::from_value::<EngineOperationKind>(value).is_err());
}
#[test]
fn operation_preparation_is_not_assignment_and_lease_receipt_remains_data() {
    let fixture:serde_json::Value=serde_json::from_str(VECTORS).unwrap();let ack=fixture["vectors"].as_array().unwrap().iter().find(|v|v["phase"]=="operation_ack").unwrap();
    let preparation:FreshPreparationData=serde_json::from_value(ack["body"]["result"].clone()).unwrap();preparation.prepare.validate().unwrap();
    assert_eq!(preparation.scope,"fresh-preparation-only");assert_eq!(preparation.assignment_epoch,"0");
    fn send_sync<T:Send+Sync>(){}send_sync::<FreshProjectPreparation>();send_sync::<EngineOperation>();
    // No AssignedProjectOwner/lease-from-readback factory exists here. Active
    // authority needs actual ORIGINAL acquire-send clock+ACK and real producer.
}

// Parser/config DATA only. No live Root/registration/process/PG permission.
use crate::root_storage_bootstrap::host_ingress::{HostReceiptStorePolicy,AllowedPreparedProject,HostRuntimeProfile,ImageRuntimeManifest,ImageRuntimeEntry};
use rudder_engine::journal::ManagedProjectPrepare;
use std::path::PathBuf;
fn operation_qualification_config_data()->HostIngressConfig {
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
fn qualification_scope_data_for_test()->crate::root_storage_bootstrap::runtime_image::qualification::RuntimeQualificationScope {
    use rudder_engine::protocol::{ObjectRef,ObjectKind};
    crate::root_storage_bootstrap::runtime_image::qualification::RuntimeQualificationScope {
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
fn disposable_runtime_qualification_refuses_real_general_operation_scope_parser() {
    let mut config=operation_qualification_config_data();
    for operation in [EngineOperationKind::PrepareFreshProjectV1{workspace_id:"prepared".into()},
        EngineOperationKind::FreshProjectStatusV1{workspace_id:"prepared".into()},
        EngineOperationKind::EnrollFreshProjectV1{workspace_id:"prepared".into()}] {
        let request=NativeRequest{version:1,request_nonce:uuid::Uuid::new_v4().to_string(),command_key:"qualification-scope-parser".into(),
            operation,constraint:FinalizationBudget::begin(Duration::from_secs(20)).unwrap().constraint().into()};
        config.runtime_qualification=None;native_scope(&request,&config).unwrap();
        config.runtime_qualification=Some(qualification_scope_data_for_test());
        assert!(native_scope(&request,&config).is_err());
    }
}
