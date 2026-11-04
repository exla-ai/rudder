//! Exercise the real native owner, App lifecycle and migration together. The
//! archive codec/export and real jj history have separate CLI fixture gates.
use super::*;
use crate::v4_migration::*;
use rudder_engine::{journal::Journal, paths::{FabricPaths, local_project_id}};
use serde_json::json;
use sha2::{Digest, Sha256};

struct Fixture { root: PathBuf, repo: PathBuf, home: PathBuf, journal: PathBuf }
impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }
impl Fixture {
    fn new() -> Self {
        let storage = if cfg!(target_os="linux") { PathBuf::from("/workspace/rudder-scratch/harden-3") }
            else { env::temp_dir() };
        let root = storage.join(format!("migration-{}", uuid::Uuid::new_v4().simple()));
        let repo = root.join("repo"); let home = root.join("home");
        fs::create_dir_all(&repo).unwrap(); fs::create_dir(&home).unwrap();
        let journal = FabricPaths::new(&home).journal;
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        Self {root, repo:repo.canonicalize().unwrap(), home, journal}
    }
    fn records(&self) -> Vec<serde_json::Value> {
        vec![json!({"id":"retained","task":"retain my source and session","status":"running",
            "backend":"codex","model":"retained-model","effort":"high",
            "workspace":{"enabled":true,"path":self.repo},
            "session":{"nativeSessionId":"original-session"},"process":{"pid":u32::MAX}})]
    }
    fn archive(&self, name: &str, checkpoint: Option<&str>) -> (PathBuf, String) {
        let bundle = self.root.join(name); fs::create_dir_all(bundle.join("objects")).unwrap();
        let blob = |bytes: &[u8]| {
            let digest = format!("{:x}",Sha256::digest(bytes));
            fs::write(bundle.join("objects").join(&digest),bytes).unwrap();
            json!({"kind":"blob","algorithm":"sha256","digest":digest,"size_bytes":bytes.len()})
        };
        let mut roots=Vec::new(); let mut entries=Vec::new();
        for name in ["integration-source","git-history","jj-history","legacy-runs"] {
            roots.push(json!({"name":name,"path":if name=="integration-source" {self.repo.clone()} else {self.repo.join(name)},"role":"source"}));
            entries.push(json!({"root":name,"path":"","type":"directory","mode":448}));
        }
        entries.push(json!({"root":"legacy-runs","path":"retained","type":"directory","mode":448}));
        entries.push(json!({"root":"legacy-runs","path":"retained/run.json","type":"file","mode":384,
            "object":blob(&serde_json::to_vec(&self.records()[0]).unwrap())}));
        roots.push(json!({"name":"session-0","path":self.root.join("session.jsonl"),"role":"sessions","run_id":"retained"}));
        entries.push(json!({"root":"session-0","path":"","type":"file","mode":384,"object":blob(b"retained transcript")}));
        let mut manifest=json!({"format_version":1,"kind":"v4_legacy_recovery","jj_version":"0.40.0",
            "project_id":local_project_id(&self.repo).unwrap(),"migration_id":"upgrade","roots":roots,"entries":entries});
        if let Some(checkpoint)=checkpoint {manifest["rollback_checkpoint"]=json!(checkpoint);}
        let bytes=serde_json::to_vec(&manifest).unwrap(); fs::write(bundle.join("manifest.json"),&bytes).unwrap();
        (bundle, format!("{:x}",Sha256::digest(bytes)))
    }
    fn committed(&self) -> MigrationReceipt {
        let mut journal=Journal::open(&self.journal).unwrap();
        let owner=journal.create_project(&local_project_id(&self.repo).unwrap(),"legacy-owner","accepted-head",1).unwrap();
        let (bundle, hash)=self.archive("original",None);
        let session=MigrationSession::open(&self.repo,&quiet()).unwrap();
        session.prepare(&journal,MigrationRequest {format_version:1,migration_id:"upgrade".into(),
            repository:self.repo.clone(),previous_authority:"legacy-native".into(),previous_owner:owner,
            next_owner_id:"fabric-migration".into(),runs:vec![LegacyRun{run_id:"retained".into(),expected_epoch:None}],
            recovery_bundle:bundle,recovery_manifest_sha256:hash}).unwrap();
        for _ in 0..16 {let receipt=session.advance(&mut journal,"upgrade",10).unwrap();
            if receipt.phase==Phase::Committed {return receipt;}}
        panic!("migration did not commit");
    }
    fn emulate_exited_process(&self) {
        // These lifecycle drills share the test runner's PID. Once its owner
        // really retired, stand in for the old OS process having exited.
        let path=self.repo.join(".rudder/engine/owner.json");
        let mut projection:serde_json::Value=serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(projection["state"],"stopped");
        // An out-of-range PID is deliberately uncertain, not proof of exit.
        let exited=i32::MAX;
        assert_eq!(unsafe {libc::kill(exited,0)},-1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(),Some(libc::ESRCH));
        projection["pid"]=json!(exited);
        fs::write(path,serde_json::to_vec(&projection).unwrap()).unwrap();
    }
}
fn quiet() -> Quiescence { Quiescence {scheduler_shutdown_confirmed:true,effects_reconciled:true,
    launchers_disabled:true,external_credentials_fenced:true} }
fn selection(receipt: &MigrationReceipt) -> AdoptionRequest { AdoptionRequest {format_version:1,
    migration_id:receipt.request.migration_id.clone(),expected_owner:receipt.owner.clone(),
    run_epochs:receipt.run_epochs.clone(),quiescence:AdoptionQuiescence {scheduler_shutdown_confirmed:true,
        effects_reconciled:true,launchers_disabled:true,external_credentials_fenced:true}} }
fn run_paused_app(owner: &mut EngineOwnership) {
    let mut lifecycle=app_engine_lifecycle::AppEngineLifecycle::prepare(owner).unwrap();
    let mut app=App::new(); app.cwd=owner.app_identity().0.into(); app.engine_headless=true;
    apply(&mut app,owner.take_migration_records().unwrap()).unwrap();
    lifecycle.start(&mut app,owner).unwrap();
    assert_eq!(app.agents.len(),1); assert_eq!(app.agents[0].status,AgentStatus::Paused);
    assert_eq!(app.agents[0].session_id.as_deref(),Some("original-session"));
    assert!(app.agents[0].terminal.is_none()); assert!(app.migration_resumes_attempted);
    lifecycle.poll(&mut app,owner).unwrap();
    assert!(app.agents[0].terminal.is_none());
    lifecycle.finish(&mut app,owner,Ok(())).unwrap();
}

#[test]
fn migration_real_owner_adoption_restart_rollback_and_fresh_recovery_lifecycle() {
    let f=Fixture::new(); let committed=f.committed();
    let mut journal=Journal::open(&f.journal).unwrap();
    let unchanged=journal.project_state(&committed.owner.project_id).unwrap();
    for mode in [NativeAuthority::Legacy,NativeAuthority::Fabric] {
        assert!(EngineOwnership::acquire_in_home(&f.repo,mode,&f.home).is_err());
        assert_eq!(journal.project_state(&committed.owner.project_id).unwrap(),unchanged);
        assert!(!f.repo.join(".rudder/engine/owner.json").exists());
    }
    let mut request=selection(&committed); request.run_epochs.clear();
    assert!(EngineOwnership::acquire_migrated_in_home(&f.repo,&f.home,&request).is_err());
    request=selection(&committed); request.quiescence.effects_reconciled=false;
    assert!(EngineOwnership::acquire_migrated_in_home(&f.repo,&f.home,&request).is_err());
    let mut owner=EngineOwnership::acquire_migrated_in_home(&f.repo,&f.home,&selection(&committed)).unwrap();
    let fence=owner.app_identity().1.clone();
    let retained=owner.source_lease(&fence).unwrap();
    assert!(MigrationSession::open(&f.repo,&quiet()).is_err());
    run_paused_app(&mut owner);
    assert!(retained.check_active_for_adoption().is_err());
    drop(owner);
    assert!(EngineOwnership::acquire_in_home(&f.repo,NativeAuthority::Fabric,&f.home).is_err());
    drop(retained); f.emulate_exited_process();
    let mut restarted=EngineOwnership::acquire_in_home(&f.repo,NativeAuthority::Fabric,&f.home).unwrap();
    assert_ne!(restarted.app_identity().1,&fence); run_paused_app(&mut restarted); drop(restarted);
    f.emulate_exited_process();
    let released=journal.project_state(&committed.owner.project_id).unwrap();
    assert!(!released.owner_active);
    let inventory=journal.recorded_run_inventory(&released.owner).unwrap().into_iter()
        .map(|run|(run.run_id,run.run_epoch)).collect();
    let session=MigrationSession::open(&f.repo,&quiet()).unwrap();
    let paused=session.pause_for_rollback_at(&mut journal,"upgrade",&released.owner,&inventory,30).unwrap();
    assert_eq!(paused.owner,released.owner);
    let (delta,hash)=f.archive("delta",paused.rollback_checkpoint.as_deref());
    assert_eq!(session.finish_rollback(&journal,"upgrade",&delta,&hash).unwrap().phase,Phase::RolledBack);
    drop(session);
    let projection=fs::read(f.repo.join(".rudder/engine/owner.json")).unwrap();
    for mode in [NativeAuthority::Legacy,NativeAuthority::Fabric] {
        assert!(EngineOwnership::acquire_in_home(&f.repo,mode,&f.home).is_err());
        assert_eq!(fs::read(f.repo.join(".rudder/engine/owner.json")).unwrap(),projection);
    }
    assert!(EngineOwnership::acquire_migrated_in_home(&f.repo,&f.home,&selection(&committed)).is_err());
    // The CLI separately verifies/export-remaps the selected delta into a NEW
    // checkout. Starting that explicitly selected checkout acquires its own
    // fresh journal authority. No marker in the old checkout is removed.
    let recovered=f.root.join("recovered"); fs::create_dir(&recovered).unwrap();
    let recovery_path=recovered.join(".rudder/engine/v4-recovery.json");
    fs::create_dir_all(recovery_path.parent().unwrap()).unwrap();
    let mut recovery=json!({"format_version":1,"repository":recovered,"source_project_id":committed.owner.project_id,
        "migration_id":"upgrade","bundle":delta,"recovery_manifest_sha256":hash,
        "rollback_checkpoint":paused.rollback_checkpoint,"run_ids":["retained"],"ready":false});
    fs::write(&recovery_path,serde_json::to_vec(&recovery).unwrap()).unwrap();
    assert!(EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Fabric,&f.home).is_err());
    assert!(EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Legacy,&f.home).is_err());
    recovery["ready"]=json!(true); fs::write(&recovery_path,serde_json::to_vec(&recovery).unwrap()).unwrap();
    assert!(EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Fabric,&f.home).is_err(),"completed export cannot omit records");
    let runs=recovered.join(".rudder/runs/retained"); fs::create_dir_all(&runs).unwrap();
    let mut record=f.records().remove(0); record["workspace"]["path"]=json!(recovered);
    record["migrationRecovery"]=json!({"session_files":["/explicitly-exported/transcript"],"original_status":"running"});
    fs::write(runs.join("run.json"),serde_json::to_vec(&record).unwrap()).unwrap();
    recovery["ready"]=json!(true); fs::write(&recovery_path,serde_json::to_vec(&recovery).unwrap()).unwrap();
    for (key,value) in [("run_ids",json!([])),("recovery_manifest_sha256",json!("0".repeat(64))),
        ("repository",json!(f.repo))] {
        let mut wrong=recovery.clone(); wrong[key]=value;
        fs::write(&recovery_path,serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Fabric,&f.home).is_err());
        assert!(!recovered.join(".rudder/engine/owner.json").exists());
    }
    fs::write(&recovery_path,serde_json::to_vec(&recovery).unwrap()).unwrap();
    let mut fresh=EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Fabric,&f.home).unwrap();
    assert_eq!(fresh.app_identity().1.epoch,1);
    let records=fresh.take_migration_records().unwrap();
    assert_eq!(records[0]["migrationRecovery"]["session_files"][0],"/explicitly-exported/transcript");
    drop(fresh);
    let current=journal.project_state(&local_project_id(&recovered).unwrap()).unwrap();
    fs::remove_file(runs.join("run.json")).unwrap();
    assert!(EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Fabric,&f.home).is_err());
    assert_eq!(journal.project_state(&current.owner.project_id).unwrap(),current);
    fs::write(runs.join("run.json"),serde_json::to_vec(&record).unwrap()).unwrap();
    let mut fresh=EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Fabric,&f.home).unwrap();
    assert_ne!(fresh.app_identity().1.project_id,committed.owner.project_id);
    assert!(fresh.app_identity().1.epoch>1);
    assert!(EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Fabric,&f.home).is_err());
    run_paused_app(&mut fresh);
    let recovered_owner=journal.project_state(&fresh.app_identity().1.project_id).unwrap().owner;
    assert_eq!(journal.recorded_run_inventory(&recovered_owner).unwrap().len(),1);
    assert!(refuse_implicit_startup(&recovered).is_err());
    drop(fresh);
    let mut fresh=EngineOwnership::acquire_in_home(&recovered,NativeAuthority::Fabric,&f.home).unwrap();
    run_paused_app(&mut fresh);
    assert!(refuse_implicit_startup(&f.repo).is_err());
    assert_eq!(journal.project_state(&committed.owner.project_id).unwrap(),released);
}

#[test]
fn migration_actual_engine_refuses_unreconciled_runtime_before_owner_publication() {
    let f=Fixture::new(); let committed=f.committed();
    let mut journal=Journal::open(&f.journal).unwrap();
    journal.observe_run_attempt(&committed.owner,"retained","uncertain-attempt",11).unwrap();
    journal.observe_runtime(&committed.owner,&rudder_engine::journal::RuntimeObservation {
        run_id:"retained".into(),run_epoch:committed.run_epochs["retained"],attempt_id:"uncertain-attempt".into(),
        backend:"codex".into(),execution_profile:"codex-app-server".into(),thread_id:Some("thread".into()),
        active_turn_id:Some("turn".into()),last_interrupted_turn_id:None,
        phase:rudder_engine::journal::RuntimePhase::Working,capabilities:["inspect".into()].into(),pty_owned:true},11).unwrap();
    let error=EngineOwnership::acquire_migrated_in_home(&f.repo,&f.home,&selection(&committed)).err().unwrap();
    assert!(error.to_string().contains("runtime custody"),"{error:#}");
    assert_eq!(journal.project_state(&committed.owner.project_id).unwrap().owner,committed.owner);
    assert!(!f.repo.join(".rudder/engine/owner.json").exists());
    assert!(MigrationSession::open(&f.repo,&quiet()).is_ok());
}

#[test]
fn migration_upgrades_a_normally_retired_legacy_app_without_restarting_its_scheduler() {
    let f=Fixture::new();
    let mut legacy=EngineOwnership::acquire_in_home(&f.repo,NativeAuthority::Legacy,&f.home).unwrap();
    let mut lifecycle=app_engine_lifecycle::AppEngineLifecycle::prepare(&mut legacy).unwrap();
    let mut app=App::new(); app.cwd=f.repo.clone(); app.engine_headless=true;
    lifecycle.start(&mut app,&mut legacy).unwrap();
    lifecycle.finish(&mut app,&mut legacy,Ok(())).unwrap();
    drop(legacy); f.emulate_exited_process();
    let mut journal=Journal::open(&f.journal).unwrap();
    let retired=journal.project_state(&local_project_id(&f.repo).unwrap()).unwrap();
    assert!(!retired.owner_active);
    let (bundle,hash)=f.archive("legacy-retired",None);
    let session=MigrationSession::open(&f.repo,&quiet()).unwrap();
    let mut receipt=session.prepare(&journal,MigrationRequest {format_version:1,migration_id:"upgrade".into(),
        repository:f.repo.clone(),previous_authority:"legacy-native".into(),previous_owner:retired.owner.clone(),
        next_owner_id:"fabric-migration".into(),runs:vec![LegacyRun {run_id:"retained".into(),expected_epoch:None}],
        recovery_bundle:bundle,recovery_manifest_sha256:hash}).unwrap();
    assert_eq!(receipt.phase,Phase::Reactivating);
    for _ in 0..16 {
        if receipt.phase==Phase::Committed {break;}
        receipt=session.advance(&mut journal,"upgrade",20).unwrap();
    }
    assert_eq!(receipt.phase,Phase::Committed);
    assert_eq!(receipt.request.previous_owner,retired.owner);
    drop(session);
    let mut fabric=EngineOwnership::acquire_migrated_in_home(&f.repo,&f.home,&selection(&receipt)).unwrap();
    run_paused_app(&mut fabric);
}

#[test]
fn migration_fabric_selection_cannot_bypass_existing_legacy_state_or_ownership() {
    let f=Fixture::new();
    let run=f.repo.join(".rudder/runs/retained"); fs::create_dir_all(&run).unwrap();
    fs::write(run.join("run.json"),serde_json::to_vec(&f.records()[0]).unwrap()).unwrap();
    let error=EngineOwnership::acquire_in_home(&f.repo,NativeAuthority::Fabric,&f.home).err().unwrap();
    assert!(error.to_string().contains("explicit migration"),"{error:#}");
    assert!(!f.repo.join(".rudder/engine/owner.json").exists());
    // Even if records disappear and a JSON authority flag says Fabric, the
    // actual legacy journal owner still requires explicit migration.
    let mut legacy=EngineOwnership::acquire_in_home(&f.repo,NativeAuthority::Legacy,&f.home).unwrap();
    legacy.finish(true).unwrap(); drop(legacy); fs::remove_dir_all(f.repo.join(".rudder/runs")).unwrap();
    let journal=Journal::open(&f.journal).unwrap();
    let previous=journal.project_state(&local_project_id(&f.repo).unwrap()).unwrap();
    let projection=f.repo.join(".rudder/engine/owner.json");
    let mut value:serde_json::Value=serde_json::from_slice(&fs::read(&projection).unwrap()).unwrap();
    value["authority"]=json!("fabric"); fs::write(&projection,serde_json::to_vec(&value).unwrap()).unwrap();
    let bytes=fs::read(&projection).unwrap();
    assert!(EngineOwnership::acquire_in_home(&f.repo,NativeAuthority::Fabric,&f.home).is_err());
    assert_eq!(journal.project_state(&previous.owner.project_id).unwrap(),previous);
    assert_eq!(fs::read(projection).unwrap(),bytes);
}
