//! Opt-in qualification of actual captured DATA using the normal transfer RPC.
use super::{new_private, write_new};
use anyhow::{ensure, Context, Result};
use rudder_engine::{
    execution::QuiescedPartialCheckpointV1,
    partial_checkpoint::{self, PartialCheckpointTemplateV1},
    paths::FabricPaths,
    protocol::canonical_json,
    service::TransferSession,
    snapshot::{RunnableRoot, SourceV2},
    store::LocalStore,
    vfs::remote_launch::RemoteLaunchRequest,
    workspace::WorkspaceProfile,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::BufReader,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    driver: PathBuf,
    driver_sha256: String,
    base_runnable: RunnableRoot,
    base_source: SourceV2,
    profile: WorkspaceProfile,
}

pub(super) fn publish(
    input: &Input,
    source: &super::Input,
    selection: RemoteLaunchRequest,
    q: &QuiescedPartialCheckpointV1,
    store_root: &Path,
    private_root: &Path,
) -> Result<serde_json::Value> {
    use std::os::unix::fs::MetadataExt;
    ensure!(input.driver == Path::new("/opt/qualification/runtime5-checkpoint-transfer.mjs"), "unexpected transfer driver");
    let metadata = fs::symlink_metadata(&input.driver)?;
    ensure!(metadata.is_file() && !metadata.file_type().is_symlink()
        && metadata.uid() == 0 && metadata.nlink() == 1 && metadata.mode() & 0o022 == 0
        && metadata.len() <= 64 * 1024, "transfer driver is not protected");
    let driver_bytes = fs::read(&input.driver)?;
    ensure!(crate::managed_launch::hex_digest(Sha256::digest(&driver_bytes)) == input.driver_sha256,
        "transfer driver differs from admitted input");
    let template = PartialCheckpointTemplateV1 {
        format_version: 1,
        selection,
        base_runnable: input.base_runnable.clone(),
        base_source: input.base_source.clone(),
        profile: input.profile.clone(),
    };
    template.validate_data()?;
    q.validate_data()?;
    let directory = private_root.join("transfer");
    new_private(&directory)?;
    // These exact original bytes survive any uncertain producer ACK below.
    write_new(&directory.join("q.json"), &canonical_json(q)?)?;
    write_new(&directory.join("template.json"), &canonical_json(&template)?)?;
    let store = LocalStore::open(store_root)?;
    let stage = store.begin_staging(Duration::from_secs(600))?;
    let prepared = partial_checkpoint::prepare_partial_checkpoint_data(&store, &stage.id, q, &template)?;
    let intent = prepared.intent()?;
    write_new(&directory.join("intent.json"), &canonical_json(&intent)?)?;
    // Exercise a lost local ACK. Never use that reply as cleanup authority.
    drop(prepared.retain(&store)?);
    let current = partial_checkpoint::reconcile_partial_checkpoint_data(&store, &intent, q, &template)?
        .context("producer DATA reconciliation found no retained bytes")?;
    let opaque_receipt = current.transfer_receipt_json()?;
    write_new(&directory.join("receipt.json"), opaque_receipt.as_bytes())?;

    // Actual managed CAS, without copying objects or allowing frames to choose
    // a path. The same RPC implementation backs the normal native helper.
    let paths = FabricPaths {
        root: directory.clone(),
        journal: directory.join("unused-journal.sqlite"),
        cas: store_root.to_path_buf(),
    };
    let session = TransferSession::open(&paths)?;
    let socket_path = private_root.join("pc.sock");
    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let bridge = directory.join("bridge");
    let script = format!("#!{}\nimport({}).then(m=>m.bridge({})).catch(()=>{{process.exitCode=1;}});\n",
        source.node.display(), serde_json::to_string(&format!("file://{}", input.driver.display()))?,
        serde_json::to_string(&socket_path)?);
    write_new(&bridge, script.as_bytes())?;
    fs::set_permissions(&bridge, fs::Permissions::from_mode(0o700))?;
    let result_path = directory.join("result.json");
    let request = serde_json::json!({"format_version":1,"base_url":source.base_url,
        "auth_headers":source.auth_headers,"scope":q.capture.base.scope,
        "publication_id":current.publication_id(),"receipt_json":opaque_receipt,
        "bridge_path":bridge,"result_path":result_path,
        "local_module_path":"/opt/rudder/dist/fabric-local.js",
        "remote_module_path":"/opt/rudder/dist/fabric-remote.js"});
    let request_path = directory.join("request.json");
    write_new(&request_path, &serde_json::to_vec(&request)?)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let server_cancel = Arc::clone(&cancel);
    let server = thread::spawn(move || -> Result<()> {
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(!server_cancel.load(Ordering::Acquire) && Instant::now() < deadline, "transfer connection deadline expired");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let reader = BufReader::new(stream.try_clone()?);
        crate::fabric_cli::rpc_session(session, reader, stream)
    });
    struct SettledChild(Child);
    impl Drop for SettledChild {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }
    let outcome = (|| -> Result<_> {
    let mut child = SettledChild(Command::new(&source.node)
        .arg(&input.driver).arg("--run").arg(&request_path)
        .env_clear().env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", &directory).env("TMPDIR", &directory)
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn()?);
    loop {
        if let Some(status) = child.0.try_wait()? { return Ok(status); }
        ensure!(Instant::now() < deadline, "checkpoint uploader deadline expired");
        thread::sleep(Duration::from_millis(5));
    }
    })();
    cancel.store(true, Ordering::Release);
    let settlement = server.join().map_err(|_| anyhow::anyhow!("transfer RPC panicked"))?;
    fs::remove_file(&socket_path)?;
    settlement?;
    let status = outcome?;
    ensure!(fs::read(&input.driver)? == driver_bytes, "transfer driver changed");
    let bytes = fs::read(&result_path)?;
    ensure!(bytes.len() <= 64 * 1024, "checkpoint proof exceeds bound");
    let result: serde_json::Value = serde_json::from_slice(&bytes)?;
    let stage = result["stage"].as_str().filter(|value| matches!(*value, "input" | "modules" | "local_rpc" | "first_upload" | "reconciliation" | "complete")).unwrap_or("unknown");
    ensure!(status.success(), "checkpoint uploader failed at {stage}");
    ensure!(result["status"] == "passed" && result["publication_id"] == current.publication_id()
        && result["local_helper_settled"] == true && result["lost_response_reconciled"] == true,
        "checkpoint transfer proof is incomplete");
    // Original capture/evidence pins stay producer-owned throughout transfer.
    ensure!(store.object_set_receipt(&q.evidence_retention.pin_id)?.as_ref() == Some(&q.evidence_retention),
        "transfer changed producer evidence retention");
    ensure!(store.object_set_receipt(&q.capture.retention.local.pin_id)?.as_ref() == Some(&q.capture.retention.local),
        "transfer changed producer capture retention");
    Ok(serde_json::json!({"format_version":1,"status":"passed","local_ack_reconciled":true,
        "original_inputs_persisted":true,"transfer_rpc_settled":true,"producer_retention_unchanged":true,
        "capture_consistency":q.capture.consistency,"publication_id":current.publication_id(),
        "publication":current.publication(),"transfer":result}))
}
