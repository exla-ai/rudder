//! Root-run only. No fake Registered/Root/source/process authority constructors.
use super::*;
const STORAGE:&str=include_str!("../tests/fixtures/engine_registration/storage-hmac-vectors.json");
#[test]
fn storage_cross_language_vectors_cover_all_five_fixed_methods() {
    let f:serde_json::Value=serde_json::from_str(STORAGE).unwrap();let mut storage=0;
    for v in f["vectors"].as_array().unwrap(){let body=&v["body"];let key=v["key_hex"].as_str().unwrap().as_bytes().chunks_exact(2).map(|x|unhex::<1>(x).unwrap()[0]).collect::<Vec<_>>();let domain=v["domain"].as_str().unwrap().as_bytes();
        assert_eq!(bytes(body).unwrap(),v["canonical_utf8"].as_str().unwrap().as_bytes());assert_eq!(sign(&key,domain,body).unwrap(),v["mac_hex"].as_str().unwrap());
        let signed=Signed {body:body.clone(),mac:v["mac_hex"].as_str().unwrap().into()};verify(&key,domain,&signed).unwrap();
        if v["phase"].as_str().unwrap().starts_with("storage_"){storage+=1;let request:OperationBody=serde_json::from_value(body.clone()).unwrap();request.operation.validate().unwrap();assert_eq!(request.payload_sha256,hex(&Sha256::digest(bytes(&request.operation).unwrap())));}
    }assert_eq!(storage,5);
}
#[test]
fn image_clock_is_separate_original_and_cancellation_is_shared() {
    assert!(ImagePreseedBudget::begin(std::time::Duration::from_secs(301)).is_err());
    assert!(FinalizationBudget::begin(std::time::Duration::from_secs(300)).is_err());
    let image=ImagePreseedBudget::begin(std::time::Duration::from_secs(300)).unwrap();let c=OperationConstraint::image(image.constraint()).unwrap();
    assert!(c.adopt(true).is_ok());assert!(c.adopt(false).is_err());let alias=image.clone();alias.cancel();assert!(image.remaining().is_err());assert!(alias.remaining().is_err());
}
#[test]
fn saved_time_is_only_data_and_invalid_wide_deadline_is_rejected() {
    let c:OperationConstraint=serde_json::from_value(serde_json::json!({"format_version":1,"maximum_milliseconds":300000,"boottime_deadline_nanoseconds":"18446744073709551616"})).unwrap();
    assert!(c.adopt(true).is_err());let c:OperationConstraint=serde_json::from_value(serde_json::json!({"format_version":1,"maximum_milliseconds":300000,"boottime_deadline_nanoseconds":"01"})).unwrap();assert!(c.adopt(true).is_err());
}
fn begin_ack()->(RegistrationAck,EngineOperationAck){
    let f:serde_json::Value=serde_json::from_str(STORAGE).unwrap();let vectors=f["vectors"].as_array().unwrap();let registered:RegistrationAck=serde_json::from_value(vectors.iter().find(|v|v["phase"]=="ack").unwrap()["body"].clone()).unwrap();
    let mut request=vectors.iter().find(|v|v["phase"]=="storage_BeginFreshRootsV1").unwrap()["body"].clone();let hash=hex(&Sha256::digest(bytes(&request).unwrap()));request.as_object_mut().unwrap().remove("remaining_milliseconds");
    request["kind"]=serde_json::json!("engine_operation_ack");request["request_sha256"]=serde_json::json!(hash);request["error"]=serde_json::Value::Null;
    request["result"]=serde_json::json!({"scope":"fresh-root-transfer-only","state":"uploading","transfer_id":"55555555-5555-4555-8555-555555555555","project_id":"66666666-6666-4666-8666-666666666666",
        "workspace_id":request["operation"]["workspace_id"],"upload_path":format!("/api/rudder/home/{}/fresh-roots/55555555-5555-4555-8555-555555555555",registered.binding.home_id),"maximum_new_objects":128,"maximum_new_bytes":4194304,"maximum_object_bytes":1048576});
    (registered,serde_json::from_value(request).unwrap())
}
#[test]
fn original_begin_scope_rejects_foreign_route_quota_and_extra_permission() {
    let (registered,ack)=begin_ack();let scope=upload_scope(&ack,&registered).unwrap();assert_eq!(scope.mode,"fresh");
    for field in ["upload_path","maximum_new_objects","retained"] {let mut bad=ack.clone();let result=bad.result.as_mut().unwrap();match field {"upload_path"=>result[field]=serde_json::json!("https://foreign.invalid/key"),"maximum_new_objects"=>result[field]=serde_json::json!(100000),_=>result[field]=serde_json::json!(true)}assert!(upload_scope(&bad,&registered).is_err());}
}
#[test]
fn no_generic_context_or_historical_enroll_becomes_original_acquire() {
    for op in [serde_json::json!({"op":"AcquireFreshOwnerV1","workspace_id":"w","transfer_id":"55555555-5555-4555-8555-555555555555","acquire_key":"k","expected_owner_epoch":1}),
        serde_json::json!({"op":"BeginFreshRootsV1","workspace_id":"w","retained":true,"context":{"trusted":true}})]{assert!(serde_json::from_value::<EngineOperationKind>(op).is_err());}
    let op=EngineOperationKind::EnrollFreshProjectV1 {workspace_id:"w".into()};assert!(!op.begin_upload());assert!(!op.image());
}
#[test]
fn image_publication_requires_actual_exact_root_receipt_fields() {
    let root=ObjectRef::from_bytes(ObjectKind::Directory,b"synthetic metadata only");let good=serde_json::json!({"format_version":1,"root_id":"r","root":root,"durability":"replicated","published_at_ms":1,"object_count":1,"total_bytes":root.size_bytes});
    serde_json::from_value::<FabricRootPublication>(good.clone()).unwrap().matches(&root).unwrap();let mut bad=good;bad["root"]["digest"]=serde_json::json!("f".repeat(64));assert!(serde_json::from_value::<FabricRootPublication>(bad).unwrap().matches(&root).is_err());
    fn send<T:Send>(){}send::<RootUploadTransfer>();send::<OriginalFreshAcquireAck>();send::<PublishedImagePreseed>();
}
