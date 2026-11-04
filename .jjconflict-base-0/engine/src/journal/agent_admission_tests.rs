//! Actual SQLite/capability mechanics. The gate is explicitly synthetic here;
//! these tests cannot construct/qualify a native ManagedOwnerLease or broker.
use super::*;
use std::{sync::atomic::AtomicUsize,io::Write};
struct Directory(std::path::PathBuf);
impl Directory {fn new()->Self{let base=if Path::new("/workspace").is_dir(){std::path::PathBuf::from("/workspace")}else{std::env::temp_dir()};let p=base.join(format!("rudder-agent-admission-test-{}",uuid::Uuid::new_v4()));std::fs::create_dir(&p).unwrap();Self(p)}}
impl Drop for Directory {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
struct Gate {state:AtomicU8,steps:AtomicUsize,writes:AtomicUsize,commits:AtomicUsize,expire_commit:bool,expire_after_commit:bool}
impl Gate {fn new()->Arc<Self>{Arc::new(Self{state:AtomicU8::new(0),steps:AtomicUsize::new(0),writes:AtomicUsize::new(0),commits:AtomicUsize::new(0),expire_commit:false,expire_after_commit:false})}}
impl AgentCommitAdmission for Gate {fn check(&self,point:AgentAdmissionPoint)->AgentAdmissionState{
    if point==AgentAdmissionPoint::SqlStep{self.steps.fetch_add(1,Ordering::AcqRel);}
    if point==AgentAdmissionPoint::SqlWrite{self.writes.fetch_add(1,Ordering::AcqRel);}
    if point==AgentAdmissionPoint::Commit{let n=self.commits.fetch_add(1,Ordering::AcqRel);if self.expire_commit||self.expire_after_commit&&n>0{self.state.store(2,Ordering::Release);}}
    match self.state.load(Ordering::Acquire){0=>AgentAdmissionState::Ready,1=>AgentAdmissionState::Busy,_=>AgentAdmissionState::Fenced}
}}
fn guarded(dir:&Directory,gate:Arc<dyn AgentCommitAdmission>)->GuardedAgentJournal {
    let j=Journal::open(dir.0.join("journal.sqlite")).unwrap();j.conn.execute_batch("CREATE TABLE admission_probe(value INTEGER)").unwrap();
    let mut raw=Some(j);let mut out=None;GuardedAgentJournal::stage_retaining(&mut out,&mut raw,gate).unwrap();assert!(raw.is_none());let mut j=out.unwrap();j.install().unwrap();j
}
fn count(j:&mut GuardedAgentJournal)->i64{j.run(AgentJournalUse::RetainedObservation,Arc::new(AtomicBool::new(false)),|j|Ok(j.conn.query_row("SELECT count(*) FROM admission_probe",[],|r|r.get(0))?)).unwrap()}

#[test] fn agent_admission_actual_commit_expiry_rolls_back(){
    let dir=Directory::new();let gate=Arc::new(Gate{expire_commit:true,..Arc::try_unwrap(Gate::new()).ok().unwrap()});let mut j=guarded(&dir,gate);
    let e=j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|j|{j.conn.execute("INSERT INTO admission_probe VALUES(1)",[])?;Ok(())}).unwrap_err();
    assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::Fenced));assert_eq!(count(&mut j),0);assert!(!j.has_unknown());
}
#[test] fn agent_admission_cached_statement_progress_fences_next_write(){
    let dir=Directory::new();let gate=Gate::new();let mut j=guarded(&dir,gate.clone());
    let e=j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|j|{
        let mut statement=j.conn.prepare_cached("INSERT INTO admission_probe VALUES(?1)")?;statement.execute([1])?;
        let writes=gate.writes.load(Ordering::Acquire);let steps=gate.steps.load(Ordering::Acquire);gate.state.store(2,Ordering::Release);
        assert!(statement.execute([2]).is_err());assert_eq!(gate.writes.load(Ordering::Acquire),writes,"same cached statement must reach progress without reauthorization");
        assert!(gate.steps.load(Ordering::Acquire)>steps);Ok(())
    }).unwrap_err();
    assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::EffectUnknown));assert!(j.has_unknown());assert_eq!(count(&mut j),1);
}
#[test] fn agent_admission_busy_then_same_live_gate_ready_is_not_permanently_fenced(){
    let dir=Directory::new();let gate=Gate::new();let mut j=guarded(&dir,gate.clone());gate.state.store(1,Ordering::Release);
    let e=j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|_|Ok(())).unwrap_err();assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::Busy));
    // Simulates pending renewal adoption finishing; never resurrects Fenced.
    gate.state.store(0,Ordering::Release);j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|j|{j.conn.execute("INSERT INTO admission_probe VALUES(1)",[])?;Ok(())}).unwrap();assert_eq!(count(&mut j),1);
}
#[test] fn agent_admission_external_writer_lock_cannot_outwait_expiry(){
    let dir=Directory::new();let gate=Gate::new();let mut j=guarded(&dir,gate.clone());let other=Connection::open(dir.0.join("journal.sqlite")).unwrap();other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let e=j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|j|{j.conn.execute("INSERT INTO admission_probe VALUES(1)",[])?;Ok(())}).unwrap_err();
    assert!(e.chain().any(|e|e.downcast_ref::<rusqlite::Error>().is_some_and(|e|matches!(e,rusqlite::Error::SqliteFailure(c,_) if c.code==rusqlite::ErrorCode::DatabaseBusy))));
    gate.state.store(2,Ordering::Release);other.execute_batch("ROLLBACK").unwrap();let e=j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|_|Ok(())).unwrap_err();assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::Fenced));assert_eq!(count(&mut j),0);
}
#[test] fn agent_admission_post_commit_unknown_keeps_observation_readonly(){
    let dir=Directory::new();let gate=Arc::new(Gate{expire_after_commit:true,..Arc::try_unwrap(Gate::new()).ok().unwrap()});let mut j=guarded(&dir,gate);
    let e=j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|j|{j.conn.execute("INSERT INTO admission_probe VALUES(1)",[])?;Ok(())}).unwrap_err();assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::EffectUnknown));assert!(j.has_unknown());assert_eq!(count(&mut j),1);
    let e=j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|_|->Result<()>{panic!("unknown original write must not replay")}).unwrap_err();assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::EffectUnknown));
    assert!(j.run(AgentJournalUse::RetainedObservation,Arc::new(AtomicBool::new(false)),|j|{j.conn.execute("INSERT INTO admission_probe VALUES(2)",[])?;Ok(())}).is_err());assert_eq!(count(&mut j),1);assert!(j.has_unknown());
}
#[test] fn agent_admission_actual_request_cancellation_reaches_commit(){
    let dir=Directory::new();let mut j=guarded(&dir,Gate::new());let cancel=Arc::new(AtomicBool::new(false));
    let e=j.run(AgentJournalUse::SharedTool,cancel.clone(),|j|{let tx=j.conn.transaction()?;tx.execute("INSERT INTO admission_probe VALUES(1)",[])?;cancel.store(true,Ordering::Release);tx.commit()?;Ok(())}).unwrap_err();assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::Fenced));assert_eq!(count(&mut j),0);
}
#[test] fn agent_admission_hook_panic_payload_survives_until_connection_owner_drop(){
    struct Payload(Arc<AtomicBool>);impl Drop for Payload{fn drop(&mut self){self.0.store(true,Ordering::Release);}}
    struct Panics(Arc<AtomicBool>);impl AgentCommitAdmission for Panics{fn check(&self,p:AgentAdmissionPoint)->AgentAdmissionState{if p==AgentAdmissionPoint::Commit{std::panic::panic_any(Payload(self.0.clone()));}AgentAdmissionState::Ready}}
    let dir=Directory::new();let dropped=Arc::new(AtomicBool::new(false));let mut j=guarded(&dir,Arc::new(Panics(dropped.clone())));
    assert!(j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|j|{j.conn.execute("INSERT INTO admission_probe VALUES(1)",[])?;Ok(())}).is_err());assert!(j.has_unknown());assert!(!dropped.load(Ordering::Acquire));
    assert!(j.run(AgentJournalUse::RetainedObservation,Arc::new(AtomicBool::new(false)),|_|->Result<()>{panic!("must not replace original panic custody")}).is_err());assert!(!dropped.load(Ordering::Acquire));drop(j);assert!(dropped.load(Ordering::Acquire));
}

struct Provider(Mutex<GuardedAgentJournal>);
impl crate::agent_api::RetainedAgentJournal for Provider {
    fn with_journal(&self,u:AgentJournalUse,c:Arc<AtomicBool>,work:&mut dyn FnMut(&mut Journal)->Result<()>)->Result<()>{self.0.lock().unwrap().run(u,c,work)}
}
#[test] fn agent_admission_retained_api_still_authenticates_exact_local_project(){
    use crate::agent_api::{AgentApi,AgentIdentityFile};use crate::journal::CapabilityGrant;use std::collections::{BTreeMap,BTreeSet};
    let dir=Directory::new();let mut journal=Journal::open(dir.0.join("journal.sqlite")).unwrap();let owner=journal.create_project("a","engine","base",0).unwrap();journal.register_run(&owner,"run",0).unwrap();
    let grant=CapabilityGrant{principal:"agent:run".into(),run_id:"run".into(),run_epoch:1,operations:BTreeSet::from(["agents:find".into()]),recipients:BTreeMap::new(),scopes:BTreeSet::new(),topics:BTreeSet::new(),expires_at_ms:None};let token=journal.issue_capability(&owner,&grant,0).unwrap();
    let identity_path=dir.0.join("identity.json");let mut identity=AgentIdentityFile{format_version:1,project_id:"b".into(),run_id:"run".into(),run_epoch:1,owner_epoch:owner.epoch,token};
    let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(&identity_path).unwrap();file.write_all(&serde_json::to_vec(&identity).unwrap()).unwrap();drop(file);
    #[cfg(unix)] {use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(&identity_path,std::fs::Permissions::from_mode(0o600)).unwrap();}
    let gate=Gate::new();let mut raw=Some(journal);let mut slot=None;GuardedAgentJournal::stage_retaining(&mut slot,&mut raw,gate.clone()).unwrap();let mut guarded=slot.unwrap();guarded.install().unwrap();let provider=Arc::new(Provider(Mutex::new(guarded)));
    assert!(AgentApi::from_retained(&identity_path,provider.clone()).is_err());identity.project_id="a".into();std::fs::write(&identity_path,serde_json::to_vec(&identity).unwrap()).unwrap();let api=AgentApi::from_retained(&identity_path,provider).unwrap();assert_eq!(api.authenticated_identity().unwrap().project_id,"a");
    gate.state.store(2,Ordering::Release);assert_eq!(api.authenticated_identity().unwrap().project_id,"a");
    let e=api.call("agents_find",serde_json::json!({})).unwrap_err();assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::Fenced));
}
#[test] fn agent_admission_unguarded_canonical_fresh_uuid_refuses_before_open(){
    let dir=Directory::new();let path=dir.0.join("must-not-be-created.sqlite");assert!(reject_unguarded_fresh_agent(&path,&uuid::Uuid::new_v4().to_string()).is_err());assert!(!path.exists());
}

#[test] fn agent_admission_inbox_expiration_is_a_guarded_write_not_observation(){
    use crate::journal::{CapabilityGrant,MessageRequest,MessageIntent,MessagePriority};use std::collections::{BTreeMap,BTreeSet};
    let dir=Directory::new();let mut journal=Journal::open(dir.0.join("journal.sqlite")).unwrap();let owner=journal.create_project("a","engine","base",0).unwrap();journal.register_run(&owner,"run",0).unwrap();
    let grant=CapabilityGrant{principal:"agent:run".into(),run_id:"run".into(),run_epoch:1,operations:BTreeSet::from(["message:send".into(),"mailbox:read".into()]),recipients:BTreeMap::from([("run".into(),1)]),scopes:BTreeSet::from(["test".into()]),topics:BTreeSet::new(),expires_at_ms:None};
    let token=journal.issue_capability(&owner,&grant,0).unwrap();
    let request=MessageRequest{idempotency_key:"one".into(),recipient_run:"run".into(),recipient_epoch:1,scope:"test".into(),intent:MessageIntent::Inform,body:serde_json::json!({"text":"fixture"}),expires_at_ms:10,priority:MessagePriority::Normal,conversation_id:None,request_id:None,reply_to:None,supersedes:None,topic_versions:BTreeMap::new(),artifacts:Vec::new()};
    let message=journal.send_message(&token,&request,0).unwrap();
    let gate=Arc::new(Gate{expire_commit:true,..Arc::try_unwrap(Gate::new()).ok().unwrap()});let mut raw=Some(journal);let mut out=None;GuardedAgentJournal::stage_retaining(&mut out,&mut raw,gate).unwrap();let mut j=out.unwrap();j.install().unwrap();
    let e=j.run(AgentJournalUse::SharedTool,Arc::new(AtomicBool::new(false)),|j|{j.inbox(&token,8,20)?;Ok(())}).unwrap_err();assert_eq!(e.downcast_ref::<AgentAdmissionError>(),Some(&AgentAdmissionError::Fenced));assert!(!j.has_unknown());
    let status:String=j.run(AgentJournalUse::RetainedObservation,Arc::new(AtomicBool::new(false)),|j|Ok(j.conn.query_row("SELECT status FROM deliveries WHERE message_id=?1",[&message.message_id],|r|r.get(0))?)).unwrap();assert_eq!(status,"pending");
    assert!(j.run(AgentJournalUse::RetainedObservation,Arc::new(AtomicBool::new(false)),|j|{j.inbox(&token,8,20)?;Ok(())}).is_err());
}
