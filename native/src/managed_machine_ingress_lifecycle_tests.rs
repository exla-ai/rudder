// Identity/projection tests only. No MachineControl, completion capsule, Root
// ACK or successful task control is reconstructed from these DATA fixtures.
use super::*;
use crate::managed_machine_receipts::{MachineCommandState,MachineReceipt};
use rudder_engine::journal::OwnerFence;
fn fixture()->(HostCommand,MachineReceipt,ManagedLaunchFence,HostOperation) {
    let target=HostCommand {version:1,session_generation:uuid::Uuid::new_v4().to_string(),rpc_id:"original-rpc".into(),command_key:"original-start".into(),
        operation:HostOperation::StartTaskV1 {project_alias:"project".into(),backend:HostBackend::Codex,model:"selected".into(),profile_id:"selected".into(),text:"task".into()},
        constraint:FinalizationBudget::begin(Duration::from_secs(20)).unwrap().constraint()};
    let receipt=MachineReceipt {command_key:target.command_key.clone(),command_id:uuid::Uuid::new_v4().to_string(),project_alias:"project".into(),
        payload_sha256:"a".repeat(64),state:MachineCommandState::Started,result:None};
    let fence=ManagedLaunchFence {owner:OwnerFence {project_id:"data-project".into(),owner_id:"data-owner".into(),epoch:1},
        run_id:"original-run".into(),run_epoch:7,attempt_id:uuid::Uuid::new_v4().to_string(),request_id:uuid::Uuid::new_v4().to_string()};
    let operation=HostOperation::StopTaskV1 {project_alias:receipt.project_alias.clone(),lookup_command_key:target.command_key.clone(),command_id:receipt.command_id.clone(),
        run_id:fence.run_id.clone(),run_epoch:fence.run_epoch.to_string(),attempt_id:fence.attempt_id.clone()};
    (target,receipt,fence,operation)
}
#[test]
fn stop_identity_refuses_each_foreign_field_before_the_app_control_route() {
    let (target,receipt,fence,operation)=fixture();assert!(stop_target_matches(&operation,&target,&receipt,&fence));
    for field in ["project_alias","lookup_command_key","command_id","run_id","run_epoch","attempt_id"] {
        let mut data=serde_json::to_value(&operation).unwrap();data[field]="foreign".into();
        let changed:HostOperation=serde_json::from_value(data).unwrap();assert!(!stop_target_matches(&changed,&target,&receipt,&fence),"{field}");
    }
    let mut foreign=receipt.clone();foreign.command_id=uuid::Uuid::new_v4().to_string();assert!(!stop_target_matches(&operation,&target,&foreign,&fence));
    let mut foreign=receipt.clone();foreign.command_key="another-start".into();assert!(!stop_target_matches(&operation,&target,&foreign,&fence));
    let mut foreign=receipt;foreign.project_alias="another-project".into();assert!(!stop_target_matches(&operation,&target,&foreign,&fence));
    let mut not_start=target;not_start.operation=HostOperation::ReceiptV1 {lookup_command_key:"original-start".into()};
    assert!(!stop_target_matches(&operation,&not_start,&foreign,&fence));
}
#[test]
fn enrollment_and_stop_data_cannot_mint_physical_join_observation() {
    let (_,_,fence,_)=fixture();let mut task=TaskWork::new();
    let mut observation=serde_json::json!({"row_status":"stopped","stop_requested_observed":true});
    append_task_lifecycle_observation(&mut observation,&task,&fence);
    assert_eq!(observation["enrollment_submitted_observed"],false);assert_eq!(observation["enrollment_released_observed"],false);
    assert!(observation["helper_joined_observed"].is_null());
    task.enrollment_submitted=true;append_task_lifecycle_observation(&mut observation,&task,&fence);
    assert_eq!(observation["enrollment_submitted_observed"],false,"an absorbing unknown/failed submit latch is not a successful queue observation");
    task.enrollment_submission_observed=true;task.released=true;task.started=Some(fence.clone());
    append_task_lifecycle_observation(&mut observation,&task,&fence);
    assert_eq!(observation["enrollment_submitted_observed"],true);assert_eq!(observation["enrollment_released_observed"],true);
    assert!(observation["helper_joined_observed"].is_null(),"release and stop are independent from the actual joined owner");
}
