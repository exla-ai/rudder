//! Strict wire DATA only: no config/process/Root/SDK capability is made here.
use super::*;
fn command()->serde_json::Value{serde_json::json!({"version":1,"session_generation":uuid::Uuid::new_v4().to_string(),"rpc_id":"rpc-1","command_key":"same-original-key",
    "operation":{"op":"BuildCurrentSourceSdkV1","project_alias":"fresh-1","case":"accepted","repeat_identical":true},"constraint":FinalizationBudget::begin(Duration::from_secs(20)).unwrap().constraint()})}
#[test]
fn sdk_command_has_no_model_selection_or_source_root_authority(){
    for case in ["accepted","proc-deny","job-deny"]{let mut raw=command();raw["operation"]["case"]=case.into();let cmd:HostCommand=serde_json::from_value(raw.clone()).unwrap();cmd.validate().unwrap();assert_eq!(serde_json::to_value(&cmd).unwrap(),raw);
        for field in ["backend","model","profile_id","source_root","runtime_ready"]{let mut wrong=raw.clone();wrong["operation"][field]="forbidden".into();assert!(serde_json::from_value::<HostCommand>(wrong).is_err());}}
}
#[test]
fn sdk_wire_refuses_unknown_cases_omitted_repeat_and_non_boolean_values(){
    let mut raw=command();raw["operation"]["case"]="arbitrary".into();assert!(serde_json::from_value::<HostCommand>(raw).is_err());
    let mut raw=command();raw["operation"].as_object_mut().unwrap().remove("repeat_identical");assert!(serde_json::from_value::<HostCommand>(raw).is_err());
    let mut raw=command();raw["operation"]["repeat_identical"]="true".into();assert!(serde_json::from_value::<HostCommand>(raw).is_err());
    assert!(serde_json::from_str::<SdkInitialSourceProfile>("\"arbitrary\"").is_err());
}
#[test]
fn same_sdk_operation_key_hash_is_independent_of_receipt_query_and_transport_clock(){
    let a:HostCommand=serde_json::from_value(command()).unwrap();let mut b=a.clone();b.rpc_id="other-rpc".into();b.session_generation=uuid::Uuid::new_v4().to_string();b.constraint=FinalizationBudget::begin(Duration::from_secs(10)).unwrap().constraint();
    assert_eq!(canonical_json(&a.operation).unwrap(),canonical_json(&b.operation).unwrap());assert_ne!(canonical_json(&a).unwrap(),canonical_json(&b).unwrap());
    let mut c=a.clone();c.operation=HostOperation::ReceiptV1{lookup_command_key:a.command_key.clone()};assert_ne!(canonical_json(&a.operation).unwrap(),canonical_json(&c.operation).unwrap());
}
