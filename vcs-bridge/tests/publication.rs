#![cfg(feature = "jj")]
mod common;
use common::*;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo;
use jj_lib::workspace::Workspace;
use rudder_vcs_bridge::{publication::*, *};

async fn prepared(repo: &std::sync::Arc<jj_lib::repo::ReadonlyRepo>) -> PreparedJjOperation {
    let mut tx = repo.start_transaction();
    let commit = tx
        .repo_mut()
        .new_commit(
            vec![
                repo.view()
                    .get_wc_commit_id(jj_lib::ref_name::WorkspaceName::DEFAULT)
                    .unwrap()
                    .clone(),
            ],
            jj_lib::merged_tree::MergedTree::resolved(
                repo.store().clone(),
                repo.store().empty_tree_id().clone(),
            ),
        )
        .set_description("exact prepared integration")
        .write()
        .await
        .unwrap();
    let identity = CommitIdentity::from_jj(&commit);
    let mut binding = binding();
    binding.baseline = identity.clone();
    binding.baseline_operation = JjOperationId(repo.op_id().hex());
    let proof = proof(binding, identity.tree.clone());
    prepare_operation(
        tx,
        "integration-1".into(),
        vec![JjOperationId(repo.op_id().hex())],
        GitCommitId(
            repo.view()
                .get_wc_commit_id(jj_lib::ref_name::WorkspaceName::DEFAULT)
                .unwrap()
                .hex(),
        ),
        identity,
        proof,
    )
    .await
    .unwrap()
}

#[test]
fn real_jj_preparation_is_unpublished_and_exact_publication_is_idempotent() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let before = repo.loader().op_heads_store().get_op_heads().await.unwrap();
        let prepared = prepared(&repo).await;
        assert_eq!(
            repo.loader().op_heads_store().get_op_heads().await.unwrap(),
            before
        );
        assert_eq!(
            inspect_operation(repo.loader(), &prepared).await.unwrap(),
            PublicationState::NotPublished
        );
        let serialized = serde_json::to_vec(&prepared).unwrap();
        let restored: PreparedJjOperation = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(
            publish_operation(repo.loader(), &restored).await.unwrap(),
            PublicationState::Published
        );
        assert_eq!(
            publish_operation(repo.loader(), &restored).await.unwrap(),
            PublicationState::Published
        );
        // Model a caller crash after jj publication but before journal receipt.
        let loader = jj_lib::repo::RepoLoader::init_from_file_system(
            &settings(),
            &dir.path().join(".jj/repo"),
            &jj_lib::repo::StoreFactories::default(),
        )
        .unwrap();
        assert_eq!(
            reconcile_operation(&loader, &restored).await.unwrap(),
            PublicationState::Published
        );
        assert_eq!(
            loader.op_heads_store().get_op_heads().await.unwrap().len(),
            1
        );
    });
}

#[test]
fn changed_op_head_is_refused_without_merge_or_replay() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let prepared = prepared(&repo).await;
        let other = repo
            .start_transaction()
            .commit("concurrent unrelated operation")
            .await
            .unwrap();
        let heads = repo.loader().op_heads_store().get_op_heads().await.unwrap();
        assert_eq!(heads, vec![other.op_id().clone()]);
        assert!(matches!(
            publish_operation(repo.loader(), &prepared).await.unwrap(),
            PublicationState::HeadChanged { .. }
        ));
        assert_eq!(
            repo.loader().op_heads_store().get_op_heads().await.unwrap(),
            heads
        );
    });
}

#[test]
fn prepared_records_cannot_change_intent_proof_or_raw_change_identity() {
    pollster::block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let (_workspace, repo) = Workspace::init_internal_git(&settings(), dir.path())
            .await
            .unwrap();
        let prepared = prepared(&repo).await;
        let mut changed = prepared.clone();
        changed.intent_id = "other-intent".into();
        assert!(publish_operation(repo.loader(), &changed).await.is_err());
        changed = prepared.clone();
        changed.proof.binding.run_epoch += 1;
        assert!(publish_operation(repo.loader(), &changed).await.is_err());
        changed = prepared.clone();
        changed.commit.change.0 = "f".repeat(32);
        assert!(publish_operation(repo.loader(), &changed).await.is_err());
        changed = prepared.clone();
        changed
            .expected_heads
            .push(changed.expected_heads[0].clone());
        assert!(changed.validate().is_err());
        assert_eq!(
            inspect_operation(repo.loader(), &prepared).await.unwrap(),
            PublicationState::NotPublished
        );
    });
}
