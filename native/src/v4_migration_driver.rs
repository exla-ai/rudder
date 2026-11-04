//! Explicit local migration control. Runs before the App exists and retains the
//! real owner lock through capture and every journal/receipt transition.
use anyhow::{ensure, Context, Result};
use rudder_engine::{journal::{Journal, OwnerFence}, paths::{FabricPaths, local_project_id}};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io::{Read, Write}, os::unix::fs::{DirBuilderExt, PermissionsExt}, path::{Path, PathBuf}, process::{Command, Stdio}};
use crate::v4_migration::{LegacyRun, MigrationRequest, MigrationSession, Phase, Quiescence};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmedQuiescence {
    scheduler_shutdown_confirmed: bool, effects_reconciled: bool,
    launchers_disabled: bool, external_credentials_fenced: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionFiles { run_id: String, files: Vec<PathBuf> }
#[derive(Deserialize)]
#[serde(tag="operation", rename_all="snake_case", deny_unknown_fields)]
enum Operation {
    Upgrade { previous_authority: String, previous_owner: OwnerFence, next_owner_id: String,
        runs: Vec<LegacyRun>, recovery_bundle: PathBuf, sessions: Vec<SessionFiles>,
        /// Explicit enrollment only. An absent project is never inferred from a receipt.
        accepted_integration_head: Option<String> },
    Advance,
    Status,
    RollbackPause { #[serde(alias="current_owner")] expected_owner: Option<OwnerFence>, run_epochs: Option<BTreeMap<String,u64>> },
    RollbackFinish { recovery_bundle: PathBuf, recovery_manifest_sha256: String },
    /// After the exact owner was paused, capture current source/session deltas,
    /// seal them against that checkpoint, then export a compatible NEW checkout.
    RollbackExport { recovery_bundle: PathBuf, sessions: Vec<SessionFiles>, destination: PathBuf },
    RecoveryExport { destination: PathBuf, rollback: bool },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request { format_version: u32, migration_id: String, quiescence: ConfirmedQuiescence, action: Operation }

// Keep the capture child inside the actual migration lock's lifetime even if
// writing its input fails. Dropping std::process::Child alone would detach it.
struct CaptureChild { child: std::process::Child, reaped: bool }
impl Drop for CaptureChild {
    fn drop(&mut self) {
        if !self.reaped { let _ = self.child.kill(); let _ = self.child.wait(); }
    }
}

pub(crate) fn read_adoption_request() -> Result<crate::v4_migration::AdoptionRequest> {
    let mut bytes=Vec::new(); std::io::stdin().take(256*1024+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len()<=256*1024,"migration adoption input exceeds bound");
    Ok(serde_json::from_slice(&bytes)?)
}

fn open_migration_journal(home: &Path) -> Result<Journal> {
    let paths=FabricPaths::new(home);
    // Existing TS users may have no Fabric directory yet. Creating private
    // storage enrolls no owner and never changes scheduler authority.
    match fs::symlink_metadata(&paths.root) {
        Ok(stat)=>ensure!(stat.is_dir() && !stat.is_symlink(),"invalid migration journal directory"),
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{
            fs::DirBuilder::new().mode(0o700).create(&paths.root)?;
            fs::File::open(home)?.sync_all()?;
        },
        Err(error)=>return Err(error.into()),
    }
    fs::set_permissions(&paths.root,fs::Permissions::from_mode(0o700))?;
    Journal::open(&paths.journal)
}

fn helper(repository: &Path, payload: &serde_json::Value) -> Result<()> {
    let bytes=serde_json::to_vec(payload)?;
    ensure!(bytes.len()<=256*1024,"migration helper input exceeds bound");
    let program=crate::cloudio::locate_rudder_cli().context("migration helper unavailable")?;
    let mut capture=CaptureChild { child:Command::new(program).arg("__migrate-v4").current_dir(repository)
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?, reaped:false };
    capture.child.stdin.take().context("migration helper input unavailable")?.write_all(&bytes)?;
    let status=capture.child.wait()?;capture.reaped=true;
    ensure!(status.success(),"migration helper failed; original state is retained");
    Ok(())
}

fn export(repository: &Path, receipt: &crate::v4_migration::MigrationReceipt,
    bundle: &Path, destination: &Path, checkpoint: Option<&str>) -> Result<()> {
    let hash = if checkpoint.is_some() {
        ensure!(receipt.rollback_bundle.as_deref() == Some(bundle), "rollback archive differs from retained receipt");
        receipt.rollback_manifest_sha256.as_deref().context("rollback archive checksum missing")?
    } else {
        ensure!(receipt.request.recovery_bundle == bundle, "recovery archive differs from retained receipt");
        &receipt.request.recovery_manifest_sha256
    };
    crate::v4_migration::verify_bundle(bundle,hash,&receipt.owner.project_id,&receipt.request.migration_id)?;
    let mut payload=serde_json::json!({"operation":"export","bundle":bundle,"destination":destination,
        "projectId":receipt.owner.project_id,"migrationId":receipt.request.migration_id});
    if let Some(checkpoint)=checkpoint { payload["rollbackCheckpoint"]=serde_json::json!(checkpoint); }
    helper(repository,&payload)
}

pub(crate) fn run(repository: &Path) -> Result<()> {
    let mut bytes=Vec::new(); std::io::stdin().take(256*1024+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len()<=256*1024,"migration input exceeds bound");
    let request:Request=serde_json::from_slice(&bytes)?;
    ensure!(request.format_version==1 && !request.migration_id.is_empty() && request.migration_id.len()<=200,
        "version-1 migration identity is required");
    let q=request.quiescence;
    let session=MigrationSession::open(repository,&Quiescence {
        scheduler_shutdown_confirmed:q.scheduler_shutdown_confirmed,effects_reconciled:q.effects_reconciled,
        launchers_disabled:q.launchers_disabled,external_credentials_fenced:q.external_credentials_fenced })?;
    let home=crate::signals::rudder_home().context("no Rudder home for migration journal")?.canonicalize()?;
    let repository=repository.canonicalize()?;
    let project=local_project_id(&repository)?;
    // Migration cannot construct a different or fresh managed namespace.
    ensure!(!crate::managed_store::has_namespace_evidence(&home,&project)?,"managed namespace requires its existing recovery path");
    let mut journal=open_migration_journal(&home)?;
    ensure!(journal.managed_project(&project)?.is_none(),"managed project requires its existing recovery path");
    let now=||std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(i64::MAX as u128) as i64;
    let receipt=match request.action {
        Operation::Upgrade {previous_authority,previous_owner,next_owner_id,runs,recovery_bundle,sessions,accepted_integration_head} => {
            ensure!(previous_owner.project_id==project && runs.len()<=1024 && sessions.len()<=1024,"migration inventory differs from this project");
            if let Some(existing)=session.read()? {
                ensure!(existing.request.migration_id==request.migration_id
                    && existing.request.previous_authority==previous_authority && existing.request.previous_owner==previous_owner
                    && existing.request.next_owner_id==next_owner_id && existing.request.runs==runs
                    && existing.request.recovery_bundle==recovery_bundle,"migration idempotency conflict");
            } else {
                // Capture goes through the TS helper while this exact native OS
                // lock remains alive. No model/network/PTY work runs here.
                let payload=serde_json::json!({"repository":repository,"bundle":recovery_bundle,
                    "projectId":project,"migrationId":request.migration_id,"sessions":sessions.into_iter().map(|s|
                        serde_json::json!({"runId":s.run_id,"files":s.files})).collect::<Vec<_>>()});
                helper(&repository,&payload)?;
                let sha256=format!("{:x}",Sha256::digest(fs::read(recovery_bundle.join("manifest.json"))?));
                match journal.project_state(&project) {
                    Ok(state)=>ensure!(state.owner==previous_owner,"stale migration enrollment"),
                    Err(error) if matches!(error.downcast_ref::<rusqlite::Error>(),Some(rusqlite::Error::QueryReturnedNoRows))=>{
                        let head=accepted_integration_head.context("explicit accepted integration head is required for enrollment")?;
                        ensure!(!head.is_empty() && head.len()<=256 && previous_owner.epoch==1,"invalid migration enrollment");
                        let created=journal.create_project(&project,&previous_owner.owner_id,&head,now())?;
                        ensure!(created==previous_owner,"migration enrollment differs");
                    },
                    Err(error)=>return Err(error),
                }
                session.prepare(&journal,MigrationRequest { format_version:1,migration_id:request.migration_id.clone(),
                    repository,previous_authority,previous_owner,next_owner_id,runs,recovery_bundle,recovery_manifest_sha256:sha256 })?;
            }
            let mut result=session.read()?.context("migration receipt missing")?;
            // Each step is independently durable. Recovery repeats only the
            // uncertain phase, never the capture or already committed imports.
            for _ in 0..1032 {
                if result.phase==Phase::Committed {
                    // Replays must validate the still-current owner and full
                    // run inventory instead of accepting an old file receipt.
                    result=session.advance(&mut journal,&request.migration_id,now())?;
                    break;
                }
                result=session.advance(&mut journal,&request.migration_id,now())?;
            }
            ensure!(result.phase==Phase::Committed,"migration exceeds bounded inventory");result
        },
        Operation::Advance=>session.advance(&mut journal,&request.migration_id,now())?,
        Operation::Status=>{let receipt=session.read()?.context("migration receipt missing")?;
            ensure!(receipt.request.migration_id==request.migration_id,"migration identity differs");receipt},
        Operation::RollbackPause {expected_owner,run_epochs}=>match (expected_owner,run_epochs) {
            (Some(owner),Some(epochs))=>session.pause_for_rollback_at(&mut journal,&request.migration_id,&owner,&epochs,now())?,
            (None,None)=>session.pause_for_rollback(&mut journal,&request.migration_id,now())?,
            _=>anyhow::bail!("rollback requires both current owner and complete run epochs"),
        },
        Operation::RollbackFinish {recovery_bundle,recovery_manifest_sha256}=>
            session.finish_rollback(&journal,&request.migration_id,&recovery_bundle,&recovery_manifest_sha256)?,
        Operation::RollbackExport {recovery_bundle,sessions,destination}=> {
            let receipt=session.read()?.context("migration receipt missing")?;
            ensure!(receipt.request.migration_id==request.migration_id
                && matches!(receipt.phase,Phase::RollbackPaused|Phase::RolledBack)
                && sessions.len()<=1024,"rollback export requires this paused migration checkpoint");
            let state=journal.project_state(&project)?;
            ensure!(!state.owner_active && state.owner==receipt.owner,"rollback owner changed");
            let actual:BTreeMap<_,_>=journal.recorded_run_inventory(&receipt.owner)?.into_iter()
                .map(|run|(run.run_id,run.run_epoch)).collect();
            ensure!(actual==receipt.run_epochs,"rollback run inventory changed");
            let checkpoint=receipt.rollback_checkpoint.as_deref().context("rollback checkpoint missing")?;
            match fs::symlink_metadata(recovery_bundle.join("manifest.json")) {
                Err(error) if error.kind()==std::io::ErrorKind::NotFound=>helper(&repository,&serde_json::json!({
                    "operation":"capture","repository":repository,"bundle":recovery_bundle,
                    "projectId":project,"migrationId":request.migration_id,"rollbackCheckpoint":checkpoint,
                    "sessions":sessions.into_iter().map(|s|serde_json::json!({"runId":s.run_id,"files":s.files})).collect::<Vec<_>>() }))?,
                Ok(stat)=>ensure!(stat.is_file() && !stat.is_symlink(),"invalid rollback archive manifest"),
                Err(error)=>return Err(error.into()),
            }
            let hash=format!("{:x}",Sha256::digest(fs::read(recovery_bundle.join("manifest.json"))?));
            let receipt=session.finish_rollback(&journal,&request.migration_id,&recovery_bundle,&hash)?;
            export(&repository,&receipt,&recovery_bundle,&destination,receipt.rollback_checkpoint.as_deref())?;
            println!("{}",serde_json::json!({"receipt":receipt,"integrationPath":destination.join("integration-source")}));
            return Ok(());
        },
        Operation::RecoveryExport {destination,rollback}=> {
            let receipt=session.read()?.context("migration receipt missing")?;
            ensure!(receipt.request.migration_id==request.migration_id,"migration identity differs");
            let (bundle,checkpoint)=if rollback {
                ensure!(receipt.phase==Phase::RolledBack,"rollback delta export is not sealed");
                (receipt.rollback_bundle.as_deref().context("rollback archive missing")?,receipt.rollback_checkpoint.as_deref())
            } else { (receipt.request.recovery_bundle.as_path(),None) };
            export(&repository,&receipt,bundle,&destination,checkpoint)?;
            println!("{}",serde_json::json!({"receipt":receipt,"integrationPath":destination.join("integration-source")}));
            return Ok(());
        },
    };
    println!("{}",serde_json::to_string(&receipt)?);Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_driver_initializes_private_journal_without_enrolling_an_owner() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {fn drop(&mut self){let _=fs::remove_dir_all(&self.0);}}
        let storage=if cfg!(target_os="linux") {PathBuf::from("/workspace/rudder-scratch/harden-3")} else {std::env::temp_dir()};
        let root=Scratch(storage.join(format!("migration-driver-{}",uuid::Uuid::new_v4().simple())));
        fs::create_dir_all(&root.0).unwrap();
        let journal=open_migration_journal(&root.0).unwrap();
        assert!(journal.project_state("not-enrolled").is_err());
        assert_eq!(fs::metadata(root.0.join("fabric")).unwrap().permissions().mode() & 0o777,0o700);
        drop(journal);
        assert!(open_migration_journal(&root.0).is_ok());
        fs::remove_dir_all(root.0.join("fabric")).unwrap();
        std::os::unix::fs::symlink(&root.0,root.0.join("fabric")).unwrap();
        assert!(open_migration_journal(&root.0).is_err());
    }

    #[test]
    fn migration_driver_accepts_typed_current_owner_alias_and_rejects_unknown_actions() {
        for key in ["expected_owner","current_owner"] {
            let mut action=serde_json::json!({"operation":"rollback_pause","run_epochs":{"retained":4}});
            action[key]=serde_json::json!({"project_id":"project","owner_id":"fabric:owner","epoch":7});
            match serde_json::from_value::<Operation>(action).unwrap() {
                Operation::RollbackPause {expected_owner:Some(owner),run_epochs:Some(epochs)}=>{
                    assert_eq!(owner.epoch,7);assert_eq!(epochs["retained"],4);
                },
                _=>panic!("typed rollback input was lost"),
            }
        }
        assert!(serde_json::from_value::<Operation>(serde_json::json!({"operation":"auto_resume"})).is_err());
    }
}
