//! Real SQLite authority/transaction tests, not compiler or Root evidence.
use super::*;
use crate::journal::{CapabilityGrant,DecisionRequest,IntegrationRequest,IntegrationEvidence};
use crate::protocol::{ObjectKind,ObjectRef};
use std::time::Duration;
struct Fixture{_dir:tempfile::TempDir,journal:Journal,owner:OwnerFence,token:String}
impl Fixture{fn new()->Self{let dir=tempfile::tempdir().unwrap();let mut journal=Journal::open(dir.path().join("journal.sqlite")).unwrap();
    let owner=journal.create_project("project","engine","base",1).unwrap();journal.register_run(&owner,"run",1).unwrap();
    let grant=CapabilityGrant{principal:"agent".into(),run_id:"run".into(),run_epoch:1,operations:BTreeSet::from(["decision:accept".into()]),recipients:BTreeMap::new(),
        scopes:BTreeSet::from(["build-policy".into()]),topics:BTreeSet::from(["old".into(),"new".into(),CANONICAL_BUILD_POLICY_TOPIC.into(),CANONICAL_DECISION_SET_TOPIC.into()]),expires_at_ms:None};
    let token=journal.issue_capability(&owner,&grant,1).unwrap();Self{_dir:dir,journal,owner,token}}
    fn clock()->FinalizationBudget{FinalizationBudget::begin(Duration::from_secs(30)).unwrap()}
    fn snapshot(&self)->CanonicalDecisionSnapshot{self.journal.capture_canonical_decisions(&self.owner,"base",&Self::clock(),&AtomicBool::new(false)).unwrap()}
    fn decide(&mut self,topic:&str,version:u64)->Result<AcceptedDecision>{self.journal.accept_decision(&self.token,&DecisionRequest{idempotency_key:format!("{topic}-{version}"),topic:topic.into(),expected_version:version,scope:"build-policy".into(),statement:serde_json::json!({"value":version})},2)}
    fn integration(&self,versions:TopicVersions)->IntegrationRequest{let source=ObjectRef::from_bytes(ObjectKind::SourceRoot,b"source-data");let environment=ObjectRef::from_bytes(ObjectKind::EnvironmentRoot,b"env-data");
        IntegrationRequest{idempotency_key:"canonical".into(),expected_head:"base".into(),new_head:"next".into(),proposed_source_root:source.clone(),proposed_environment_root:environment.clone(),versions,
            evidence:IntegrationEvidence{tested_head:"base".into(),tested_source_root:source,tested_environment_root:environment,action_results:vec![],checks:serde_json::json!({"component":"journal-vector-only"})}}}
}
#[test]fn canonical_vector_rejects_new_topic_at_final_transaction_and_leaves_head_unchanged(){let mut f=Fixture::new();f.decide("old",0).unwrap();let snap=f.snapshot();
    assert_eq!(snap.versions_data()["old"],1);assert_eq!(snap.versions_data()[CANONICAL_BUILD_POLICY_TOPIC],0);f.decide("new",0).unwrap();
    assert!(snap.assert_current(&f.journal,&Fixture::clock(),&AtomicBool::new(false)).is_err());let request=f.integration(snap.versions_data().clone());
    assert!(f.journal.integrate(&f.owner,&request,3).is_err());assert_eq!(f.journal.project_state("project").unwrap().integration_head,"base");
    assert_eq!(f.journal.conn.query_row("SELECT count(*) FROM integrations",[],|r|r.get::<_,i64>(0)).unwrap(),0);
}
#[test]fn canonical_max_sequence_detects_older_writer_without_a_new_schema_counter(){let f=Fixture::new();let snap=f.snapshot();
    // This models the actual old writer SQL: no canonical helper/counter call.
    let old=Connection::open(f.journal.database_path().unwrap()).unwrap();old.execute_batch("BEGIN IMMEDIATE").unwrap();
    old.execute("INSERT INTO decisions(decision_id,project_id,topic,version,principal,scope,statement_json,previous_id,sequence,idempotency_key,request_json) VALUES('legacy-decision','project','legacy-new',1,'legacy','build-policy','{}',NULL,123,'legacy-key','{}')",[]).unwrap();
    old.execute("INSERT INTO topics(project_id,topic,version,decision_id) VALUES('project','legacy-new',1,'legacy-decision')",[]).unwrap();old.execute_batch("COMMIT").unwrap();
    assert_eq!(decision_set_revision(&f.journal.conn,"project").unwrap(),123);assert!(snap.assert_current(&f.journal,&Fixture::clock(),&AtomicBool::new(false)).is_err());
}
#[test]fn canonical_policy_capture_and_reserved_peer_topic_have_distinct_authority(){let mut f=Fixture::new();assert!(f.snapshot().policy_decision_data().is_none());
    let accepted=f.decide(CANONICAL_BUILD_POLICY_TOPIC,0).unwrap();let snap=f.snapshot();assert_eq!(snap.policy_decision_data(),Some(&accepted));
    assert!(f.decide(CANONICAL_DECISION_SET_TOPIC,0).is_err());snap.assert_current(&f.journal,&Fixture::clock(),&AtomicBool::new(false)).unwrap();
    f.decide(CANONICAL_BUILD_POLICY_TOPIC,1).unwrap();assert!(snap.assert_current(&f.journal,&Fixture::clock(),&AtomicBool::new(false)).is_err());
}
#[test]fn canonical_snapshot_refuses_cancelled_capture_and_changed_owner_or_head(){let f=Fixture::new();let clock=Fixture::clock();clock.cancel();
    assert!(f.journal.capture_canonical_decisions(&f.owner,"base",&clock,&AtomicBool::new(false)).is_err());let snap=f.snapshot();
    f.journal.conn.execute("UPDATE projects SET integration_head='changed' WHERE project_id='project'",[]).unwrap();assert!(snap.assert_current(&f.journal,&Fixture::clock(),&AtomicBool::new(false)).is_err());
    f.journal.conn.execute("UPDATE projects SET integration_head='base',owner_epoch=owner_epoch+1 WHERE project_id='project'",[]).unwrap();assert!(snap.assert_current(&f.journal,&Fixture::clock(),&AtomicBool::new(false)).is_err());
}
