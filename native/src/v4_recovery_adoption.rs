//! Deliberate handoff into a separately exported recovery checkout. This file
//! never grants authority: only EngineOwnership's held lock and live Journal do.
use anyhow::{ensure, Context, Result};
use rudder_engine::journal::{Journal, OwnerFence};
use serde::Deserialize;
use std::{collections::BTreeSet, fs, path::{Path, PathBuf}};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportedRecovery {
    format_version: u32,
    repository: PathBuf,
    source_project_id: String,
    migration_id: String,
    bundle: PathBuf,
    recovery_manifest_sha256: String,
    rollback_checkpoint: Option<String>,
    run_ids: Vec<String>,
    ready: bool,
}

/// Off App, under the fresh checkout's actual owner lock, before owner changes.
/// Legacy admission keeps refusing even after successful Fabric adoption.
pub(crate) fn prepare(repository: &Path, journal: &Journal) -> Result<Option<Vec<serde_json::Value>>> {
    let path=repository.join(".rudder/engine/v4-recovery.json");
    let stat=match fs::symlink_metadata(&path) {
        Ok(stat)=>stat,
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),
        Err(error)=>return Err(error.into()),
    };
    ensure!(stat.is_file() && !stat.is_symlink() && stat.len()<=256*1024,"invalid recovery handoff record");
    let receipt:ExportedRecovery=serde_json::from_slice(&fs::read(path)?)?;
    ensure!(receipt.format_version==1 && receipt.ready && receipt.repository==repository
        && receipt.run_ids.len()<=1024 && receipt.run_ids.iter().all(|id|crate::v4_migration::safe_run_id(id))
        && receipt.run_ids.iter().collect::<BTreeSet<_>>().len()==receipt.run_ids.len(),
        "recovery export is incomplete or belongs to another checkout");
    crate::v4_migration::verify_bundle(&receipt.bundle,&receipt.recovery_manifest_sha256,
        &receipt.source_project_id,&receipt.migration_id)?;
    let manifest:serde_json::Value=serde_json::from_slice(&fs::read(receipt.bundle.join("manifest.json"))?)?;
    ensure!(manifest["rollback_checkpoint"].as_str()==receipt.rollback_checkpoint.as_deref(),
        "recovery archive differs from its paused checkpoint");
    let retained:BTreeSet<_>=manifest["entries"].as_array().context("recovery inventory missing")?.iter()
        .filter(|entry|entry["root"]=="legacy-runs" && entry["type"]=="file")
        .filter_map(|entry|entry["path"].as_str()?.strip_suffix("/run.json")).collect();
    ensure!(retained==receipt.run_ids.iter().map(String::as_str).collect(),"recovery omitted archived run history");
    let runs=repository.join(".rudder/runs");
    let mut records=Vec::new(); let mut found=BTreeSet::new();
    match fs::symlink_metadata(&runs) {
        Ok(stat)=>{
            ensure!(stat.is_dir() && !stat.is_symlink(),"linked recovery run store");
            for entry in fs::read_dir(&runs)? {
                let entry=entry?;
                ensure!(entry.file_type()?.is_dir(),"invalid recovery run directory");
                let id=entry.file_name().into_string().map_err(|_|anyhow::anyhow!("invalid recovery run identity"))?;
                ensure!(crate::v4_migration::safe_run_id(&id) && records.len()<1024,"recovery run inventory exceeds bound");
                let path=entry.path().join("run.json"); let stat=fs::symlink_metadata(&path)?;
                ensure!(stat.is_file() && !stat.is_symlink() && stat.len()<=2*1024*1024,"invalid recovery run record");
                let record=serde_json::from_slice(&fs::read(path)?)?;
                records.push(crate::v4_migration::compatible_record(&id,record)?); found.insert(id);
            }
        },
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},
        Err(error)=>return Err(error.into()),
    }
    ensure!(receipt.run_ids.iter().all(|id|found.contains(id)),"recovery lost a retained run record");
    let project=rudder_engine::paths::local_project_id(repository)?;
    ensure!(project!=receipt.source_project_id,"recovery handoff requires a separate exported checkout");
    match journal.project_state(&project) {
        Ok(state)=>{
            ensure!(state.owner.owner_id.starts_with("fabric:"),"recovery cannot replace an existing legacy journal owner");
            ensure!(journal.recorded_run_inventory(&state.owner)?.iter().all(|run|found.contains(&run.run_id)),
                "recovery lost a current journal run record");
        },
        Err(error) if matches!(error.downcast_ref::<rusqlite::Error>(),Some(rusqlite::Error::QueryReturnedNoRows))=>{},
        Err(error)=>return Err(error),
    }
    Ok(Some(records))
}

/// Under the new active fence. Import registers history only, with no runtime,
/// capability, PTY or permission to resume. Restart can repeat registration.
pub(crate) fn import(journal: &mut Journal, owner: &OwnerFence, records: &[serde_json::Value], now: i64) -> Result<()> {
    let known:BTreeSet<_>=journal.recorded_run_inventory(owner)?.into_iter().map(|run|run.run_id).collect();
    for record in records {
        let id=record["id"].as_str().context("recovery run identity missing")?;
        if !known.contains(id) {journal.register_run(owner,id,now)?;}
    }
    Ok(())
}
