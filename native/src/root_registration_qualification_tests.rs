//! Pure cryptographic/schema refusals, not a captured Root key/registration,
//! manufactured successful ACK or provider qualification.
use super::*;

#[test]
fn qualification_mac_domains_cannot_be_replaced_by_registration_or_ack_domains() {
    let fixture_key=[7u8;32];let data=serde_json::json!({"fixture":"no authority"});
    let packet=Signed{body:data.clone(),mac:sign(&fixture_key,REQUEST,&data).unwrap()};
    verify(&fixture_key,REQUEST,&packet).unwrap();
    for domain in [RESPONSE,super::super::PROOF,super::super::ACK]{assert!(verify(&fixture_key,domain,&packet).is_err());}
    let changed=Signed{body:serde_json::json!({"fixture":"changed"}),mac:packet.mac.clone()};
    assert!(verify(&fixture_key,REQUEST,&changed).is_err());
}

#[test]
fn malformed_qualification_ack_shape_never_decodes() {
    for raw in [br#"{"ready":true}"#.as_slice(),br#"{"version":1,"kind":"runtime_image_qualification_ack","result":{"state":"published"}}"#.as_slice()]{
        assert!(serde_json::from_slice::<AckBody>(raw).is_err());
    }
}

#[test]
fn qualification_request_body_cannot_be_replaced_by_a_general_operation() {
    let operation=serde_json::json!({"version":1,"kind":"engine_operation","command_key":"fixture","operation":"start"});
    assert!(serde_json::from_value::<RequestBody>(operation).is_err());
    assert_ne!(REQUEST,super::super::PROOF);
    assert_ne!(RESPONSE,super::super::ACK);
}
