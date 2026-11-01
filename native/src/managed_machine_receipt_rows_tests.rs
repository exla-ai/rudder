//! Actual SQLite transactions and canonical receipt DATA only. Fixtures do not
//! construct HostIngressConfigOwner/AcceptedHostCommand or claim Root authority.
use super::*;
fn fixture()->Connection {let conn=Connection::open_in_memory().unwrap();conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();for(_,sql)in TABLES {conn.execute_batch(sql).unwrap();}conn}
fn command(alias:&str)->HostCommand {HostCommand {version:1,session_generation:uuid::Uuid::new_v4().to_string(),rpc_id:"rpc-1".into(),command_key:"client-7".into(),
    operation:HostOperation::StartTaskV1 {project_alias:alias.into(),backend:rudder_native::host_ingress::HostBackend::Codex,model:"explicit-model".into(),profile_id:"selected-api".into(),text:"first\n\tsecond".into()},
    constraint:FinalizationBudget::begin(Duration::from_secs(20)).unwrap().constraint()}}
fn insert(conn:&mut Connection,command:&HostCommand)->MachineReceipt {let body=String::from_utf8(canonical_json(&command.operation).unwrap()).unwrap();let digest=format!("{:x}",Sha256::digest(body.as_bytes()));let receipt=MachineReceipt {command_key:command.command_key.clone(),command_id:uuid::Uuid::new_v4().to_string(),project_alias:operation_alias(&command.operation).unwrap().into(),payload_sha256:digest.clone(),state:MachineCommandState::Accepted,result:None};insert_accepted(conn,command,&receipt,&body,&digest,"actual-config-hash-DATA").unwrap();receipt}
#[test]
fn accepted_and_pending_effect_are_one_atomic_transaction() {let mut conn=fixture();conn.execute_batch("CREATE TRIGGER fail_effect BEFORE INSERT ON effects BEGIN SELECT RAISE(ABORT,'fault'); END").unwrap();
    let c=command("project-a");let body=String::from_utf8(canonical_json(&c.operation).unwrap()).unwrap();let receipt=MachineReceipt {command_key:c.command_key.clone(),command_id:uuid::Uuid::new_v4().to_string(),project_alias:"project-a".into(),payload_sha256:format!("{:x}",Sha256::digest(body.as_bytes())),state:MachineCommandState::Accepted,result:None};
    assert!(insert_accepted(&mut conn,&c,&receipt,&body,&receipt.payload_sha256,"config").is_err());assert!(lookup(&conn,&c.command_key).unwrap().is_none());}
#[test]
fn starting_wrong_original_rolls_back_effect_and_retry_cannot_restart() {let mut conn=fixture();let c=command("project-a");let receipt=insert(&mut conn,&c);let mut foreign=receipt.clone();foreign.command_id=uuid::Uuid::new_v4().to_string();assert!(start_original(&mut conn,&foreign).is_err());
    let state:String=conn.query_row("SELECT state FROM effects",[],|r|r.get(0)).unwrap();assert_eq!(state,"pending");start_original(&mut conn,&receipt).unwrap();assert!(start_original(&mut conn,&receipt).is_err());assert_eq!(lookup(&conn,&c.command_key).unwrap().unwrap().0.state,MachineCommandState::Started);}
#[test]
fn same_client_key_is_immutable_across_session_or_project_payload_changes() {let mut conn=fixture();let c=command("project-a");let receipt=insert(&mut conn,&c);let mut retry=c.clone();retry.session_generation=uuid::Uuid::new_v4().to_string();retry.rpc_id="rpc-new".into();
    let row=lookup(&conn,&retry.command_key).unwrap().unwrap();assert_eq!(row.0.command_id,receipt.command_id);assert_eq!(row.1, String::from_utf8(canonical_json(&retry.operation).unwrap()).unwrap());
    let changed=command("project-b");assert_ne!(row.1,String::from_utf8(canonical_json(&changed.operation).unwrap()).unwrap());let changed_body=String::from_utf8(canonical_json(&changed.operation).unwrap()).unwrap();let changed_hash=format!("{:x}",Sha256::digest(changed_body.as_bytes()));assert!(insert_accepted(&mut conn,&changed,&receipt,&changed_body,&changed_hash,"config").is_err());assert_eq!(lookup(&conn,&c.command_key).unwrap().unwrap().0.project_alias,"project-a");}
#[test]
fn completed_receipt_is_task_admission_data_and_cannot_start_again() {let mut conn=fixture();let c=command("project-a");let receipt=insert(&mut conn,&c);start_original(&mut conn,&receipt).unwrap();finish_original(&mut conn,&receipt,"{\"phase\":\"task_queued\"}".into()).unwrap();
    let row=lookup(&conn,&c.command_key).unwrap().unwrap().0;assert_eq!(row.state,MachineCommandState::Completed);assert_eq!(row.result.unwrap()["phase"],"task_queued");assert!(start_original(&mut conn,&receipt).is_err());assert!(lookup(&conn,"absent-original-key").unwrap().is_none());}
#[test]
fn corrupt_saved_body_or_hash_never_becomes_a_valid_receipt() {let mut conn=fixture();let c=command("project-a");insert(&mut conn,&c);conn.execute("UPDATE commands SET body_hash=?1",["0".repeat(64)]).unwrap();assert!(lookup(&conn,&c.command_key).is_err());}
#[test]
fn fresh_status_reads_advance_without_replacing_the_original_accepted_receipt() {
    let mut conn=fixture();let c=command("project-a");let original=insert(&mut conn,&c);
    assert_eq!(lookup(&conn,&c.command_key).unwrap().unwrap().0.state,MachineCommandState::Accepted);
    start_original(&mut conn,&original).unwrap();assert_eq!(lookup(&conn,&c.command_key).unwrap().unwrap().0.state,MachineCommandState::Started);
    finish_original(&mut conn,&original,"{\"phase\":\"task_queued\"}".into()).unwrap();let current=lookup(&conn,&c.command_key).unwrap().unwrap().0;
    assert_eq!(current.state,MachineCommandState::Completed);assert_eq!(current.command_id,original.command_id);assert_eq!(original.state,MachineCommandState::Accepted);assert!(original.result.is_none());
}
