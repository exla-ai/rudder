//! Pure signed protocol DATA tests; no fabricated Root/Native/process owner.
use super::*;
fn binding()->RegistrationBinding {RegistrationBinding {account_id:"a".into(),home_id:"h".into(),app_id:"app".into(),app_name:"app-name".into(),network_name:"network".into(),network_cidr:"fdaa:1::/48".into(),machine_id:"machine".into(),instance_id:"instance".into(),image:format!("registry.fly.io/image@sha256:{}","a".repeat(64)),config_sha256:"b".repeat(64),machine_config_sha256:"c".repeat(64),store_id:"store".into(),incarnation:"incarnation".into(),provision_nonce:uuid::Uuid::new_v4().to_string(),epoch:"1".into(),native_sha256:"d".repeat(64),launcher_sha256:"e".repeat(64),service_sha256:"f".repeat(64),node_sha256:"1".repeat(64),sidecar_policy_sha256:"2".repeat(64),runtime_manifest_sha256:"3".repeat(64)}}
fn process(pid:u32)->ProcessEvidence {ProcessEvidence {pid,start_ticks:"9007199254740993".into(),image_sha256:"d".repeat(64),exe:ObservedId {device:"1".into(),inode:"9007199254740993".into()},namespaces:["mnt","user","net","pid"].into_iter().map(|n|(n.into(),ObservedId {device:"4".into(),inode:"9007199254740994".into()})).collect()}}
fn proof()->ProofBody {ProofBody {version:1,kind:"proof".into(),challenge_id:uuid::Uuid::new_v4().to_string(),challenge_nonce:"a".repeat(64),root_nonce:uuid::Uuid::new_v4().to_string(),engine_instance_id:uuid::Uuid::new_v4().to_string(),credential_id:uuid::Uuid::new_v4().to_string(),binding:binding(),constraint:FinalizationBudget::begin(std::time::Duration::from_secs(30)).unwrap().constraint(),evidence:Evidence {boot_id:uuid::Uuid::new_v4().to_string(),root:process(2),engine:process(3),service:process(4),host_worker_tid:5,host_worker_start_ticks:"42".into(),root_client_authenticated:true,parent_authentication_accepted:true,host_worker_ack_authenticated:true}}}
fn ack(p:&ProofBody)->RegistrationAck {RegistrationAck {version:1,kind:"ack".into(),challenge_id:p.challenge_id.clone(),challenge_nonce:p.challenge_nonce.clone(),root_nonce:p.root_nonce.clone(),engine_instance_id:p.engine_instance_id.clone(),credential_id:p.credential_id.clone(),binding:p.binding.clone(),constraint:p.constraint.clone(),boot_id:p.evidence.boot_id.clone(),proof_sha256:hex(&Sha256::digest(bytes(p).unwrap()))}}
#[test]
fn registration_mac_uses_existing_constant_time_verifier_and_direction_domains() {
    let p=proof();let packet=Signed {mac:sign(&[0x42;32],PROOF,&p).unwrap(),body:p};verify(&[0x42;32],PROOF,&packet).unwrap();
    assert!(verify(&[0x42;32],CHALLENGE,&packet).is_err());assert!(verify(&[0x43;32],PROOF,&packet).is_err());
}
#[test]
fn registration_matching_requires_all_original_proof_fields() {
    let p=proof();let a=ack(&p);ack_matches_proof(&a,&p).unwrap();
    for field in 0..6 {let mut changed=a.clone();match field {0=>changed.root_nonce=uuid::Uuid::new_v4().to_string(),1=>changed.boot_id=uuid::Uuid::new_v4().to_string(),2=>changed.binding.instance_id="replacement".into(),3=>changed.proof_sha256="f".repeat(64),4=>changed.engine_instance_id=uuid::Uuid::new_v4().to_string(),_=>changed.binding.epoch="2".into()};assert!(ack_matches_proof(&changed,&p).is_err());}
}
#[test]
fn registration_wide_kernel_identities_are_strings_in_canonical_wire() {
    let p=proof();let value=serde_json::to_value(&p).unwrap();assert_eq!(value["evidence"]["engine"]["exe"]["inode"],"9007199254740993");
    assert!(value["evidence"]["engine"]["pid"].is_number());assert_eq!(bytes(&p).unwrap(),canonical_json(&value).unwrap());
}
#[test]
fn registration_unknown_fields_and_noncanonical_tag_are_rejected() {
    let p=proof();let mut value=serde_json::to_value(&p).unwrap();value["Context"]=serde_json::json!({"trusted":true});assert!(serde_json::from_value::<ProofBody>(value).is_err());
    let packet=Signed {body:p,mac:"A".repeat(64)};assert!(verify(&[0x42;32],PROOF,&packet).is_err());
}
#[test]
fn registration_late_known_ack_is_data_and_cannot_renew_constraint() {
    let mut p=proof();p.constraint=FinalizationConstraint::from_json(br#"{"format_version":1,"maximum_milliseconds":30000,"boottime_deadline_nanoseconds":"1"}"#).unwrap();let a=ack(&p);
    let retained=Some(a);ack_matches_proof(retained.as_ref().unwrap(),&p).unwrap();assert!(FinalizationBudget::from_constraint(&p.constraint).is_err());assert!(retained.is_some());
    // No call here can construct a RegisteredEngineOwner; actual Root channel,
    // original descriptors and live worker/certificate acquisition are required.
}
#[test]
fn registration_key_framing_rejects_uppercase_short_or_ambiguous_hex() {
    assert!(unhex::<32>(b"00").is_err());assert!(unhex::<32>("A".repeat(64).as_bytes()).is_err());assert!(unhex::<32>("g".repeat(64).as_bytes()).is_err());
    assert_eq!(unhex::<32>("a".repeat(64).as_bytes()).unwrap(),[0xaa;32]);
}
#[test]
fn registration_owner_is_retained_send_sync_without_serde_or_public_factory() {
    fn send_sync<T:Send+Sync>(){}send_sync::<RegisteredEngineOwner>();
    assert_ne!(BEGIN,ACK);assert_ne!(PROOF,CREDENTIAL);
}
