//! Refusal/wire tests only. No synthetic value constructs a native fresh lease.
use super::*;
#[test]
fn fresh_broker_admission_errors_keep_typed_busy_fenced_and_unknown(){
    use rudder_engine::journal::AgentAdmissionError::*;
    for (state,code) in [(Busy,"busy"),(Fenced,"fenced"),(EffectUnknown,"effect_unknown")]{
        let error:anyhow::Error=state.into();let wire=wire_error(&error);assert_eq!(wire.code,code);
        assert_eq!(retryable(&problem(&wire.code,&wire.message)),state==Busy);
    }
}
#[test]
fn fresh_broker_legacy_factory_refuses_uuid_before_creating_endpoint_or_journal(){
    let base=if Path::new("/workspace").is_dir(){PathBuf::from("/workspace")}else{std::env::temp_dir()};
    let root=base.join(format!("rudder-fresh-broker-refusal-{}",uuid::Uuid::new_v4()));fs::create_dir(&root).unwrap();
    let identity=root.join("identity.json");let data=AgentIdentityFile{format_version:1,project_id:uuid::Uuid::new_v4().to_string(),run_id:"run".into(),run_epoch:1,owner_epoch:1,token:"f".repeat(64)};
    let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&identity).unwrap();file.write_all(&serde_json::to_vec(&data).unwrap()).unwrap();drop(file);
    let socket=root.join("must-not-exist/broker.sock");let journal=root.join("must-not-exist.sqlite");
    assert!(AgentBroker::spawn(BrokerConfig{journal_path:journal.clone(),identity_file:identity,socket_path:socket.clone()}).is_err());
    assert!(!socket.parent().unwrap().exists());assert!(!journal.exists());fs::remove_dir_all(root).unwrap();
}
