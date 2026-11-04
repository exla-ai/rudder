use super::*;
use serde_json::json;

struct Scratch(PathBuf);
impl Drop for Scratch { fn drop(&mut self) { let _=fs::remove_dir_all(&self.0); } }
fn quiet() -> Quiescence {
    Quiescence {scheduler_shutdown_confirmed:true,effects_reconciled:true,
        launchers_disabled:true,external_credentials_fenced:true}
}
#[test]
fn migration_adoption_requires_exact_committed_inventory_and_keeps_recovery_paused() {
    let root=(if std::path::Path::new("/workspace").is_dir(){PathBuf::from("/workspace/rudder-scratch/n13")}else{std::env::temp_dir().canonicalize().unwrap().join("rudder-n13")}).join(format!("migration-adoption-{}",uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap(); let _scratch=Scratch(root.clone());
    let repository=root.join("repo");fs::create_dir(&repository).unwrap();
    let repository=repository.canonicalize().unwrap();
    let project=rudder_engine::paths::local_project_id(&repository).unwrap();
    let mut journal=Journal::open(&root.join("journal.sqlite")).unwrap();
    let owner=journal.create_project(&project,"legacy","accepted-head",1).unwrap();
    let bundle=root.join("archive");fs::create_dir_all(bundle.join("objects")).unwrap();
    let source=json!({"id":"retained","task":"retain original task","status":"running",
        "backend":"codex","model":"retained-model","session":{"nativeSessionId":"retained-session"},
        "worktree":{"enabled":true,"path":"/retained-workspace"},"process":{"pid":u32::MAX}});
    let blob=|bytes:&[u8]| {
        let digest=format!("{:x}",Sha256::digest(bytes));
        fs::write(bundle.join("objects").join(&digest),bytes).unwrap();
        json!({"kind":"blob","algorithm":"sha256","digest":digest,"size_bytes":bytes.len()})
    };
    let mut roots=Vec::new();let mut entries=Vec::new();
    for name in ["integration-source","git-history","jj-history","legacy-runs"] {
        roots.push(json!({"name":name,"path":repository.join(name),"role":"source"}));
        entries.push(json!({"root":name,"path":"","type":"directory","mode":448}));
    }
    entries.push(json!({"root":"legacy-runs","path":"retained","type":"directory","mode":448}));
    entries.push(json!({"root":"legacy-runs","path":"retained/run.json","type":"file","mode":384,"object":blob(&serde_json::to_vec(&source).unwrap())}));
    roots.push(json!({"name":"session-0","path":"/retained-session","role":"sessions","run_id":"retained"}));
    entries.push(json!({"root":"session-0","path":"","type":"file","mode":384,"object":blob(b"retained transcript")}));
    let manifest=json!({"format_version":1,"kind":"v4_legacy_recovery","jj_version":"0.40.0",
        "project_id":project,"migration_id":"adoption","roots":roots,"entries":entries});
    let manifest=serde_json::to_vec(&manifest).unwrap();fs::write(bundle.join("manifest.json"),&manifest).unwrap();
    let session=MigrationSession::open(&repository,&quiet()).unwrap();
    session.prepare(&journal,MigrationRequest {format_version:1,migration_id:"adoption".into(),
        repository:repository.clone(),previous_authority:"legacy-native".into(),previous_owner:owner,
        next_owner_id:"fabric-migration".into(),runs:vec![LegacyRun {run_id:"retained".into(),expected_epoch:None}],
        recovery_bundle:bundle.clone(),recovery_manifest_sha256:format!("{:x}",Sha256::digest(&manifest))}).unwrap();
    let mut committed=None;
    for _ in 0..16 {
        let receipt=session.advance(&mut journal,"adoption",10).unwrap();
        if receipt.phase==Phase::Committed { committed=Some(receipt);break; }
    }
    let receipt=committed.unwrap();
    let mut request=AdoptionRequest {format_version:1,migration_id:"adoption".into(),
        expected_owner:receipt.owner.clone(),run_epochs:receipt.run_epochs.clone(),
        quiescence:AdoptionQuiescence {scheduler_shutdown_confirmed:true,effects_reconciled:true,
            launchers_disabled:true,external_credentials_fenced:true}};
    // This is the same live OS fence the actual EngineOwnership constructor
    // retains before projecting records; the migration helper cannot compete.
    assert!(MigrationSession::open(&repository,&quiet()).is_err());
    let projected=prepare_adoption(&repository,&journal,&request).unwrap();
    assert_eq!(projected[0]["status"],"paused");assert!(projected[0].get("process").is_none());
    assert_eq!(projected[0]["session"],source["session"]);
    assert_eq!(projected[0]["workspace"],source["worktree"]);
    assert_eq!(projected[0]["migrationRecovery"]["automatic_resume"],false);
    assert!(refuse_implicit_startup(&repository).is_err());
    request.run_epochs.clear();assert!(prepare_adoption(&repository,&journal,&request).is_err());
    request.run_epochs=receipt.run_epochs.clone();
    request.quiescence.effects_reconciled=false;assert!(prepare_adoption(&repository,&journal,&request).is_err());
    request.quiescence.effects_reconciled=true;
    journal.release_drained_owner(&receipt.owner,20).unwrap();
    assert!(prepare_adoption(&repository,&journal,&request).is_err());
    request.expected_owner=journal.project_state(&project).unwrap().owner;
    request.run_epochs=journal.recorded_run_inventory(&request.expected_owner).unwrap().into_iter()
        .map(|run|(run.run_id,run.run_epoch)).collect();
    assert!(!journal.project_state(&project).unwrap().owner_active);
    assert_eq!(prepare_adoption(&repository,&journal,&request).unwrap()[0]["status"],"paused");
    let current=repository.join(".rudder/runs/retained");fs::create_dir_all(&current).unwrap();
    let mut updated=source.clone();updated["task"]=json!("latest post-upgrade task");updated["process"]=serde_json::Value::Null;
    fs::write(current.join("run.json"),serde_json::to_vec(&updated).unwrap()).unwrap();
    assert_eq!(prepare_adoption(&repository,&journal,&request).unwrap()[0]["task"],"latest post-upgrade task");
    assert_eq!(fs::read(bundle.join("manifest.json")).unwrap(),manifest);
    assert!(prepare_fabric_restart(&repository,&journal).is_err());
    let adopted=journal.transfer_recovered_owner(&request.expected_owner,"fabric:adopted",30).unwrap();
    // Transferring alone (or inventing owner.json) is not adoption evidence.
    assert!(prepare_fabric_restart(&repository,&journal).is_err());
    record_adoption(&repository,&mut journal,&adopted).unwrap();
    record_adoption(&repository,&mut journal,&adopted).unwrap();
    assert_eq!(prepare_fabric_restart(&repository,&journal).unwrap().unwrap()[0]["status"],"paused");
    assert!(refuse_implicit_startup(&repository).is_err());
    journal.release_drained_owner(&adopted,31).unwrap();
    assert_eq!(prepare_fabric_restart(&repository,&journal).unwrap().unwrap()[0]["task"],"latest post-upgrade task");
    let released=journal.project_state(&project).unwrap().owner;
    let restarted=journal.transfer_recovered_owner(&released,"fabric:restarted",32).unwrap();
    // Death between transfer and adoption receipt conservatively requires the
    // explicit adoption path, rather than trusting a prior epoch's receipt.
    assert!(prepare_fabric_restart(&repository,&journal).is_err());
    record_adoption(&repository,&mut journal,&restarted).unwrap();
    assert!(prepare_fabric_restart(&repository,&journal).unwrap().is_some());
    let mut wrong=receipt.clone();wrong.request.migration_id="different-migration".into();session.save(&wrong).unwrap();
    assert!(prepare_fabric_restart(&repository,&journal).is_err());session.save(&receipt).unwrap();
    wrong=receipt.clone();wrong.phase=Phase::RollbackPaused;session.save(&wrong).unwrap();
    assert!(prepare_fabric_restart(&repository,&journal).is_err());session.save(&receipt).unwrap();
    journal.observe_run_attempt(&restarted,"retained","crashed-attempt",33).unwrap();
    journal.observe_runtime(&restarted,&rudder_engine::journal::RuntimeObservation {
        run_id:"retained".into(),run_epoch:journal.run_epoch(&project,"retained").unwrap(),attempt_id:"crashed-attempt".into(),
        backend:"codex".into(),execution_profile:"codex-app-server".into(),thread_id:Some("thread".into()),
        active_turn_id:Some("turn".into()),last_interrupted_turn_id:None,
        phase:rudder_engine::journal::RuntimePhase::Working,capabilities:["inspect".into()].into(),pty_owned:true},33).unwrap();
    journal.recover_runtime_restart(&restarted,34).unwrap();
    // Receipt admission still only returns paused projections. The real host's
    // required recovery/transfer rejects unresolved physical runtime custody.
    assert_eq!(prepare_fabric_restart(&repository,&journal).unwrap().unwrap()[0]["status"],"paused");
    assert!(journal.transfer_recovered_owner(&restarted,"fabric:forbidden",35).is_err());
    let mut changed=manifest.clone();changed.push(b' ');fs::write(bundle.join("manifest.json"),changed).unwrap();
    assert!(prepare_adoption(&repository,&journal,&request).is_err());
}
