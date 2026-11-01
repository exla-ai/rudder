//! DATA/parser regressions only. No fake Root/registered/image/lease owner.
use super::*;
use crate::host_ingress::{HostImagePreseedPolicy,HostReceiptStorePolicy,HostRuntimeProfile,ImageRuntimeManifest,AllowedPreparedProject};
use rudder_engine::journal::ManagedProjectNamespace;
fn fixture()->(EngineOperationAck,HostIngressConfig){
    let vectors:serde_json::Value=serde_json::from_str(include_str!("../tests/fixtures/engine_registration/hmac-vectors.json")).unwrap();
    let mut ack:EngineOperationAck=serde_json::from_value(vectors["vectors"].as_array().unwrap().iter().find(|v|v["phase"]=="operation_ack").unwrap()["body"].clone()).unwrap();
    let project=uuid::Uuid::new_v4().to_string();let old=uuid::Uuid::new_v4().to_string();let transfer=uuid::Uuid::new_v4().to_string();
    let manifest=ObjectRef::from_bytes(ObjectKind::Blob,b"DATA manifest");let installer=ObjectRef::from_bytes(ObjectKind::Directory,b"DATA installer");
    ack.operation=EngineOperationKind::BeginImagePreseedV1{workspace_id:"fixture".into(),manifest:manifest.clone(),installer_input:installer.clone(),image_receipt_sha256:"e".repeat(64)};
    let publication=|root:&ObjectRef,id:String|serde_json::json!({"format_version":1,"root_id":id,"root":root,"durability":"replicated","published_at_ms":1,"object_count":1,"total_bytes":root.size_bytes});
    ack.result=Some(serde_json::json!({"scope":"image-preseed-reuse-only","state":"published","transfer_id":transfer,"source_transfer_id":old,"project_id":project,"workspace_id":"fixture","installer_input":installer,"manifest_publication":publication(&manifest,format!("managed-image-manifest-{old}")),"publication":publication(&installer,format!("managed-image-{old}"))}));ack.error=None;
    let home=std::path::PathBuf::from("/workspace/private/fixture");let checkout=home.join("checkout");
    let namespace=ManagedProjectNamespace{format_version:1,project_id:project.clone(),home:home.clone(),journal:home.join("fabric/journal.sqlite"),repository:checkout.clone(),directory:ManagedProjectNamespace::directory_for(&home,&project)};
    let config=HostIngressConfig{sdk_initial_source:None,compiler_sdk:None,compiler_qualification:None,runtime_qualification:None,seed_profile:None,version:1,machine_id:"fixture".into(),store_id:uuid::Uuid::new_v4().to_string(),incarnation:uuid::Uuid::new_v4().to_string(),account_id:"fixture-account".into(),home_id:"fixture-home".into(),cloud_origin:"https://rudder-cloud-control.fly.dev".into(),state_directory:home.clone(),store_policy:HostReceiptStorePolicy::InitializeEmptyV1,
        projects:vec![AllowedPreparedProject{alias:"fixture".into(),checkout,home:home.clone(),cas:home.join("cas"),endpoints:home.join("endpoints"),prepare:ManagedProjectPrepare{request_id:uuid::Uuid::new_v4().to_string(),namespace}}],
        runtime_profile:HostRuntimeProfile::FlyManagedImageV1{user_name:"Fixture".into(),user_email:"fixture@example.invalid".into()},runtime_manifest:ImageRuntimeManifest{profile_id:"fly-managed-image-v1".into(),manifest_sha256:"a".repeat(64),entries:Vec::new()},owner_mode:None,owner_lease_safety:None,
        image_preseed:Some(HostImagePreseedPolicy{image:"registry.fly.io/fixture@sha256:".to_owned()+&"b".repeat(64),image_receipt_sha256:"e".repeat(64),full_manifest_sha256:manifest.digest,batch_transport:None,runtime_mount:None})};
    (ack,config)
}
#[test]
fn image_reuse_schema_distinguishes_current_begin_from_cold_upload_data(){let(ack,config)=fixture();assert!(has_image_reuse_result(&ack));image_reuse_result(&ack,&config).unwrap();
    let vectors:serde_json::Value=serde_json::from_str(include_str!("../tests/fixtures/engine_registration/hmac-vectors.json")).unwrap();
    let registration:RegistrationAck=serde_json::from_value(vectors["vectors"].as_array().unwrap().iter().find(|v|v["phase"]=="ack").unwrap()["body"].clone()).unwrap();assert!(upload_scope(&ack,&registration).is_err());}
#[test]
fn image_reuse_refuses_changed_graph_policy_project_pins_and_extra_fields(){let(ack,config)=fixture();for change in 0..7{let mut a=ack.clone();let r=a.result.as_mut().unwrap();match change{0=>r["installer_input"]["digest"]=serde_json::json!("f".repeat(64)),1=>r["project_id"]=serde_json::json!(uuid::Uuid::new_v4().to_string()),2=>r["source_transfer_id"]=serde_json::json!(uuid::Uuid::new_v4().to_string()),3=>r["manifest_publication"]["root_id"]=serde_json::json!("foreign-pin"),4=>r["publication"]["durability"]=serde_json::json!("local"),5=>r["retained"]=serde_json::json!(true),_=>r["state"]=serde_json::json!("uploading")};assert!(image_reuse_result(&a,&config).is_err());}
    let mut c=config.clone();c.image_preseed.as_mut().unwrap().image_receipt_sha256="f".repeat(64);assert!(image_reuse_result(&ack,&c).is_err());
}
#[test]
fn image_reuse_data_cannot_be_source_or_historical_publish_ack(){let(mut ack,config)=fixture();ack.operation=EngineOperationKind::PublishImagePreseedV1{workspace_id:"fixture".into(),transfer_id:uuid::Uuid::new_v4().to_string()};assert!(!has_image_reuse_result(&ack));assert!(image_reuse_result(&ack,&config).is_err());}
