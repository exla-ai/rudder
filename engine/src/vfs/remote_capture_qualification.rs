//! Mounted generated-public-data proof, separate from the product capture API.
use super::*;
use rudder_engine::vfs::{PartialCaptureBudget, PartialCaptureReceipt, PartialCaptureRequest};

struct CompleteRpcMustNotAuthorize;
impl vfs::rpc::AttemptAuthority for CompleteRpcMustNotAuthorize {
    fn validate(&self, _: &vfs::rpc::ObservationBinding) -> Result<()> {
        bail!("partial view reached complete-observation authorization")
    }
}
fn complete_rpc_rejected(view: &VfsView, session: &RemoteSession, path: &Path) -> Result<()> {
    let profile = canonical_json(view.manager().remote_profile().context("remote profile")?)?;
    let config = vfs::rpc::RpcConfig {
        attempt: vfs::rpc::AttemptContext {
            execution_id: session.attempt().execution_id.clone(),
            operation_id: session.attempt().operation_id.clone(),
        },
        profile: ObjectRef::from_bytes(ObjectKind::Blob, &profile),
        authority: Arc::new(CompleteRpcMustNotAuthorize),
        adoption_witness: None,
        limits: vfs::rpc::RpcLimits::default(),
    };
    let failure = vfs::rpc::ObservationRpc::bind(view.clone(), config, path)
        .err()
        .context("partial view enabled complete RPC")?;
    anyhow::ensure!(
        vfs::errno(&failure) == libc::EOPNOTSUPP && !path.exists(),
        "complete RPC did not fail before endpoint/profile publication"
    );
    Ok(())
}
fn captured_file(
    manager: &VfsManager,
    receipt: &PartialCaptureReceipt,
    name: &[u8],
) -> Result<ObjectRef> {
    let plan = rudder_engine::delta::read_partial_tree(manager.store(), &receipt.retention)?;
    let adapter = directory::LocalDirectoryStore::read_only(manager.store());
    let entry = PagedDirectory::new(&adapter)
        .lookup(&plan.directory, name)?
        .context("captured file")?;
    let EntryNode::File { object, .. } = entry.node else {
        bail!("captured entry is not file")
    };
    Ok(object)
}
pub(super) fn qualify_capture(
    manager: &VfsManager,
    view: &VfsView,
    session: &RemoteSession,
    graph: &Graph,
    file: &fs::File,
    sibling: &fs::File,
) -> Result<(serde_json::Value, PartialCaptureReceipt)> {
    let before_pins = manager.store().retention()?.pins;
    let before_private = log_bytes(manager, view)?;
    complete_rpc_rejected(view, session, &manager.root().join("complete-rpc-before"))?;
    let request = PartialCaptureRequest {
        nonce: Uuid::new_v4().to_string(),
        expected_begin: view.dirty_cursor()?,
        expected_head: None,
    };
    let pin = format!("vfs:partial:{}:{}", view.id(), request.nonce);
    let cancel_after_data_ack = || {
        manager
            .store()
            .object_set_receipt(&pin)
            .map_or(true, |r| r.is_some())
    };
    let started = Instant::now();
    let initial = view.capture_partial_source_cancellable(
        &ImportPolicy::default(),
        &request,
        &PartialCaptureBudget::default(),
        &cancel_after_data_ack,
    );
    anyhow::ensure!(
        initial.is_err(),
        "lost-ACK fixture did not withhold the head reply"
    );
    let first_ms = started.elapsed().as_millis();
    let status = view
        .inspect_partial_capture(&request.nonce)?
        .context("retained candidate")?;
    anyhow::ensure!(
        status.state == "retained"
            && status.retention.is_some()
            && status.receipt.is_none()
            && !status.current
            && view.partial_capture_head()?.is_none(),
        "object-set ACK was confused with a committed capture head"
    );
    manager.store().gc(Duration::ZERO)?;
    // The same actual mounted FD stays open across capture and another write.
    file.write_all_at(b"later!!", 11)?;
    file.sync_all()?;
    let mut actual = [0; 7];
    file.read_exact_at(&mut actual, 11)?;
    anyhow::ensure!(
        &actual == b"later!!",
        "held FD later bytes were not observed"
    );
    sibling.read_exact_at(&mut actual, 11)?;
    anyhow::ensure!(&actual == b"qqqqqqq", "capture changed sibling data");
    let later = view.dirty_cursor()?;
    anyhow::ensure!(
        later.generation > request.expected_begin.generation,
        "later FD write has no dirty witness"
    );
    let replay = view.capture_partial_source(
        &ImportPolicy::default(),
        &request,
        &PartialCaptureBudget::default(),
    )?;
    anyhow::ensure!(
        Some(&replay.retention) == status.retention.as_ref()
            && replay.begin == request.expected_begin
            && replay.end == replay.begin
            && replay.head_sequence == 1,
        "retry changed the original capture or guessed a writer barrier"
    );
    let retained_file = captured_file(manager, &replay, &graph.large_name)?;
    anyhow::ensure!(
        rudder_engine::content::read_file_range(manager.store(), &retained_file, 11, 7)?
            == b"changed",
        "old nonce recaptured later FD bytes"
    );
    anyhow::ensure!(
        rudder_engine::content::read_file_range(
            manager.store(),
            &retained_file,
            CHUNK_BYTES as u64 + 5,
            4
        )? == b"MMAP",
        "capture lost the actual mmap bytes"
    );
    manager.store().gc(Duration::ZERO)?;
    anyhow::ensure!(
        view.capture_partial_source(
            &ImportPolicy::default(),
            &request,
            &PartialCaptureBudget::default()
        )? == replay,
        "acknowledged nonce replay changed after GC"
    );
    let plan = rudder_engine::delta::read_partial_tree(manager.store(), &replay.retention)?;
    let blob_bytes: u64 = plan
        .local_objects
        .iter()
        .filter(|r| r.kind == ObjectKind::Blob)
        .map(|r| r.size_bytes)
        .sum();
    anyhow::ensure!(
        plan.local_objects.len() < 128
            && plan.remote_boundaries.len() < 1024
            && blob_bytes <= 9 * 1024 * 1024
            && !plan.remote_boundaries.is_empty(),
        "capture did not remain a bounded dirty-chunk/radix producer"
    );
    anyhow::ensure!(
        manager.store().retained_closure(&plan.directory).is_err(),
        "partial tree became complete local availability"
    );
    let next_request = PartialCaptureRequest {
        nonce: Uuid::new_v4().to_string(),
        expected_begin: later,
        expected_head: Some(request.nonce.clone()),
    };
    let next = view.capture_partial_source(
        &ImportPolicy::default(),
        &next_request,
        &PartialCaptureBudget::default(),
    )?;
    anyhow::ensure!(
        next.head_sequence == 2 && next.previous_head == Some(request.nonce.clone()),
        "capture head CAS chain differs"
    );
    view.release_partial_capture(&request.nonce)?;
    manager.store().gc(Duration::ZERO)?;
    anyhow::ensure!(
        manager
            .store()
            .object_set_refs(&replay.retention.local)
            .is_err(),
        "retired generation remains active"
    );
    let current_file = captured_file(manager, &next, &graph.large_name)?;
    anyhow::ensure!(
        rudder_engine::content::read_file_range(manager.store(), &current_file, 11, 7)?
            == b"later!!",
        "prior candidate retirement removed current head data"
    );
    anyhow::ensure!(
        view.release_partial_capture(&next_request.nonce).is_err(),
        "current head release was accepted"
    );
    complete_rpc_rejected(view, session, &manager.root().join("complete-rpc-after"))?;
    anyhow::ensure!(
        vfs::errno(&view.checkpoint_snapshot_unmounted().unwrap_err()) == libc::EOPNOTSUPP
            && vfs::errno(
                &view
                    .observe_source(
                        &ImportPolicy::default(),
                        &Uuid::new_v4().to_string(),
                        &ObservationBudget::default()
                    )
                    .unwrap_err()
            ) == libc::EOPNOTSUPP,
        "partial capture enabled complete checkpoint/observation"
    );
    let descriptor = view.descriptor()?;
    anyhow::ensure!(
        descriptor.base == graph.directory
            && descriptor.latest.is_none()
            && descriptor.latest_snapshot.is_none()
            && descriptor.partial_capture_format == Some(1)
            && manager.store().retention()?.pins == before_pins,
        "partial retention rebound the mutable base or minted a complete pin"
    );
    Ok((
        serde_json::json!({
            "first_capture_ms":first_ms,"new_reachable_objects":plan.local_objects.len(),
            "remote_boundaries":plan.remote_boundaries.len(),"new_blob_bytes":blob_bytes,"dirty_4mib_chunks":2,
            "private_bytes_before_later_fd_write":before_private,"private_bytes_after_later_fd_write":log_bytes(manager,view)?,
            "begin_generation":replay.begin.generation,"later_generation":next.begin.generation,"namespace_format":4,
            "data_ack_without_head":true,"same_nonce_after_live_write_and_gc":true,"actual_mmap_bytes_retained":true,
            "head_cas_and_retirement":true,"current_head_release_rejected":true,"complete_rpc_errno":libc::EOPNOTSUPP,
            "complete_checkpoint_errno":libc::EOPNOTSUPP,"complete_observation_errno":libc::EOPNOTSUPP,
            "baseline_unchanged":true,"new_complete_pins":0,"live_best_effort_only":true
        }),
        next,
    ))
}
