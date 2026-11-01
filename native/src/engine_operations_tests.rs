//! Actual canonical/HMAC functions against independent synthetic vectors. No
//! live Owner/Root credential/process/PG/source permission is reconstructed.
use super::*;
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
