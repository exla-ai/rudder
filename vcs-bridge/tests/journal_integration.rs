#![cfg(feature = "jj")]
mod common;
use common::settings;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::{RefName, WorkspaceName};
use jj_lib::repo::Repo;
use jj_lib::workspace::Workspace;
use rudder_engine::journal::*;
use rudder_engine::protocol::{CheckpointConsistency, ObjectKind, OwnerEpoch};
use rudder_engine::snapshot::{EnvironmentV2, RunnableRoot, SourceV2};
use rudder_engine::store::{
    DirectoryEntry, DirectoryManifest, EntryNode, LocalStore, ReferencedMetadata,
};
use rudder_vcs_bridge::projection::{project_source, verify_versioned_source};
use rudder_vcs_bridge::projection_io::ProjectionBudget;
use rudder_vcs_bridge::{journal_adapter::*, publication::*, *};
use std::collections::BTreeMap;
use std::time::Duration;

struct LoseReceipt<'a> {
    inner: JournalJjPublisher<'a>,
    calls: usize,
}
impl VcsPublisher for LoseReceipt<'_> {
    fn inspect(
        &mut self,
        record: &PreparedVcsOperation,
    ) -> anyhow::Result<VcsPublicationObservation> {
        self.inner.inspect(record)
    }
    fn publish(
        &mut self,
        record: &PreparedVcsOperation,
    ) -> anyhow::Result<VcsPublicationObservation> {
        self.calls += 1;
        assert_eq!(
            self.inner.publish(record)?,
            VcsPublicationObservation::Published
        );
        anyhow::bail!("receipt lost after real jj operation publication")
    }
}

#[test]
fn real_jj_operation_publishes_through_retained_journal_intent_and_reconciles_lost_receipt() {
    for (verified_case, lose_receipt) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        // All work is inside this private fixture; no user/main history opens.
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("private-integration");
        std::fs::create_dir(&repo_dir).unwrap();
        let (_workspace, repo) =
            pollster::block_on(Workspace::init_internal_git(&settings(), &repo_dir)).unwrap();
        let baseline = repo
            .store()
            .get_commit(
                repo.view()
                    .get_wc_commit_id(WorkspaceName::DEFAULT)
                    .unwrap(),
            )
            .unwrap();
        let base = baseline.id().hex();
        let store = LocalStore::open(dir.path().join("cas")).unwrap();
        let entries = if verified_case {
            vec![
                DirectoryEntry {
                    name_hex: hex::encode("café source.ts"),
                    node: EntryNode::File {
                        object: store.put_blob(b"export const n = 7;\n").unwrap(),
                        mode: 0o644,
                    },
                },
                DirectoryEntry {
                    name_hex: hex::encode("run"),
                    node: EntryNode::File {
                        object: store.put_blob(b"#!/bin/sh\necho verified\n").unwrap(),
                        mode: 0o755,
                    },
                },
                DirectoryEntry {
                    name_hex: hex::encode("link"),
                    node: EntryNode::Symlink {
                        target_hex: hex::encode("café source.ts"),
                    },
                },
            ]
        } else {
            vec![]
        };
        let directory = store
            .put_directory(DirectoryManifest {
                format_version: 1,
                mode: 0o755,
                entries,
            })
            .unwrap();
        let policy = store
            .put_blob(b"strict exact versioned source policy")
            .unwrap();
        let profile = store.put_blob(b"controlled private view profile").unwrap();
        let topology = store
            .put_blob(br#"{"format_version":1,"groups":[]}"#)
            .unwrap();
        let source = store
            .put_metadata(
                ObjectKind::SourceRoot,
                &SourceV2 {
                    format_version: 2,
                    consistency: CheckpointConsistency::Quiesced,
                    directory: directory.clone(),
                    projection_policy: policy.clone(),
                },
            )
            .unwrap();
        let target_tree = if verified_case {
            pollster::block_on(project_source(
                store.clone(),
                &directory,
                &baseline.tree(),
                &policy,
                None,
                None,
                ProjectionBudget::default(),
            ))
            .unwrap()
            .tree
        } else {
            baseline.tree()
        };
        let verification_stage = store.begin_staging(Duration::from_secs(600)).unwrap();
        let verified = if verified_case {
            Some(
                pollster::block_on(verify_versioned_source(
                    store.clone(),
                    &verification_stage.id,
                    &source,
                    &policy,
                    &target_tree,
                    None,
                    ProjectionBudget::default(),
                ))
                .unwrap(),
            )
        } else {
            None
        };
        let environment = store
            .put_metadata(
                ObjectKind::EnvironmentRoot,
                &EnvironmentV2 {
                    format_version: 2,
                    directory: directory.clone(),
                    platform: [("abi".into(), "test".into())].into_iter().collect(),
                    installer_inputs: vec![],
                    provenance: BTreeMap::new(),
                },
            )
            .unwrap();
        let recovery = store
            .put_metadata(
                ObjectKind::RunnableRoot,
                &RunnableRoot {
                    format_version: 1,
                    consistency: CheckpointConsistency::Quiesced,
                    source: source.clone(),
                    environment: Some(environment.clone()),
                    view_root: directory.clone(),
                    profile: profile.clone(),
                    hardlinks: topology.clone(),
                    knowledge_cursor: 0,
                },
            )
            .unwrap();
        let mut journal = Journal::open(dir.path().join("journal.sqlite")).unwrap();
        let owner = journal
            .create_project("project", "owner", &base, 1000)
            .unwrap();
        journal
            .observe_run_attempt(&owner, "source", "attempt", 1000)
            .unwrap();
        let request = VcsIntentRequest {
            idempotency_key: "land-real-jj".into(),
            source_run: VcsRunFence {
                run_id: "source".into(),
                run_epoch: 1,
                attempt_id: "attempt".into(),
                view_id: "view".into(),
            },
            expected_head: base.clone(),
            proposed_source_root: source.clone(),
            proposed_environment_root: environment.clone(),
            recovery_root: recovery.clone(),
            seed_root: directory.clone(),
            projection_policy: policy.clone(),
            profile: profile.clone(),
            expected_view_root: Some(directory.clone()),
            hardlinks: Some(topology),
            versions: BTreeMap::new(),
            evidence: IntegrationEvidence {
                tested_head: base.clone(),
                tested_source_root: source.clone(),
                tested_environment_root: environment.clone(),
                action_results: verified.iter().map(|v| v.evidence().clone()).collect(),
                // Mapping is separate from these fixture verification facts;
                // the mapping receipt is never a claim that tests executed.
                checks: serde_json::json!({"controlled_fixture":"verified","nonempty":verified_case}),
            },
        };
        let accepted = journal
            .begin_vcs_intent(&owner, &store, &request, 1000)
            .unwrap();
        assert_eq!(accepted.state, VcsIntentState::Accepted);
        if let Some(verified) = &verified {
            verified.release_after_journal_retention().unwrap();
        }
        store.abandon_staging(&verification_stage.id).unwrap();
        // Prepare AFTER the durable accepted intent. This uses real jj objects,
        // a fresh change identity, and preserves the actual canonical ancestor.
        let mut tx = repo.start_transaction();
        let commit = pollster::block_on(
            tx.repo_mut()
                .new_commit(vec![baseline.id().clone()], target_tree)
                .set_description("real integrated child")
                .write(),
        )
        .unwrap();
        let binding = AttemptBinding {
            format_version: 1,
            project_id: "project".into(),
            owner: OwnerEpoch {
                owner: owner.owner_id.clone(),
                epoch: owner.epoch,
            },
            run_id: "source".into(),
            run_epoch: 1,
            attempt_id: "attempt".into(),
            view_id: "view".into(),
            seed: directory,
            baseline: CommitIdentity::from_jj(&baseline),
            baseline_operation: JjOperationId(repo.op_id().hex()),
            projection_policy: policy,
            profile,
        };
        let proof = SealedSourceProof {
            format_version: 1,
            binding,
            recovery_root: recovery,
            source,
            environment: Some(environment),
            tree: JjTreeIdentity::from_jj(&commit.tree()),
        };
        let prepared = if let Some(verified) = &verified {
            pollster::block_on(prepare_verified_operation(
                tx,
                accepted.intent_id.clone(),
                vec![JjOperationId(repo.op_id().hex())],
                GitCommitId(base.clone()),
                CommitIdentity::from_jj(&commit),
                proof,
                verified,
            ))
            .unwrap()
        } else {
            pollster::block_on(prepare_operation(
                tx,
                accepted.intent_id.clone(),
                vec![JjOperationId(repo.op_id().hex())],
                GitCommitId(base.clone()),
                CommitIdentity::from_jj(&commit),
                proof,
            ))
            .unwrap()
        };
        assert_eq!(prepared.format_version, if verified_case { 2 } else { 1 });
        if !verified_case {
            let old_format_golden = serde_json::json!({
                "format_version":1,"intent_id":prepared.intent_id,"expected_heads":prepared.expected_heads,
                "operation":prepared.operation,"integration_base":prepared.integration_base,
                "commit":prepared.commit,"proof":prepared.proof,
            });
            assert_eq!(
                rudder_engine::protocol::canonical_json(&prepared).unwrap(),
                rudder_engine::protocol::canonical_json(&old_format_golden).unwrap()
            );
            assert!(
                serde_json::to_value(&prepared)
                    .unwrap()
                    .get("projection_receipt")
                    .is_none()
            );
        }
        let staging = store.begin_staging(Duration::from_secs(60)).unwrap();
        if let Some(verified) = &verified {
            let mut wrong = prepared.clone();
            wrong.projection_receipt = None;
            assert!(wrong.validate().is_err());
            let mut metadata: ReferencedMetadata =
                serde_json::from_slice(&store.read_object(verified.evidence()).unwrap()).unwrap();
            metadata.payload["receipt"]["tree"] =
                serde_json::to_value(JjTreeIdentity::from_jj(&baseline.tree())).unwrap();
            let substituted = store
                .put_metadata(ObjectKind::ActionResult, &metadata)
                .unwrap();
            store
                .pin("substituted-fixture-receipt", &substituted)
                .unwrap();
            wrong = prepared.clone();
            wrong.projection_receipt = Some(substituted);
            assert!(persist_prepared_record(&store, &staging.id, &wrong).is_err());
        }
        let envelope = persist_prepared_record(&store, &staging.id, &prepared).unwrap();
        journal
            .attach_vcs_operation(&owner, &store, &accepted.intent_id, &envelope, 1000)
            .unwrap();
        store.abandon_staging(&staging.id).unwrap();
        let result = if lose_receipt {
            let mut publisher = LoseReceipt {
                inner: JournalJjPublisher {
                    loader: repo.loader(),
                    store: &store,
                },
                calls: 0,
            };
            let uncertain = journal
                .publish_vcs_intent(&owner, &accepted.intent_id, &mut publisher, 1000)
                .unwrap();
            assert_eq!(uncertain.state, VcsIntentState::Uncertain);
            assert_eq!(
                journal.project_state("project").unwrap().integration_head,
                base
            );
            let next_owner = journal.transfer_owner(&owner, "new-owner", 1000).unwrap();
            let receipt = journal
                .reconcile_vcs_intent(&next_owner, &accepted.intent_id, &mut publisher, 1000)
                .unwrap();
            assert_eq!(publisher.calls, 1);
            receipt
        } else {
            journal
                .publish_vcs_intent(
                    &owner,
                    &accepted.intent_id,
                    &mut JournalJjPublisher {
                        loader: repo.loader(),
                        store: &store,
                    },
                    1000,
                )
                .unwrap()
        };
        assert_eq!(result.state, VcsIntentState::Published);
        assert_eq!(
            journal.project_state("project").unwrap().integration_head,
            commit.id().hex()
        );
        let visible = pollster::block_on(repo.loader().load_at_head()).unwrap();
        assert_eq!(
            visible
                .view()
                .get_local_bookmark(RefName::new(INTEGRATION_BOOKMARK))
                .as_normal(),
            Some(commit.id())
        );
        assert_eq!(
            CommitIdentity::from_jj(&visible.store().get_commit(baseline.id()).unwrap()),
            CommitIdentity::from_jj(&baseline)
        );
    }
}
