#![allow(dead_code)]
use rudder_engine::protocol::{ObjectKind, ObjectRef, OwnerEpoch};
use rudder_vcs_bridge::*;

pub fn reference(kind: ObjectKind, label: &str) -> ObjectRef {
    ObjectRef::from_bytes(kind, label.as_bytes())
}
pub fn commit() -> CommitIdentity {
    CommitIdentity {
        commit: GitCommitId("a".repeat(40)),
        change: JjChangeId("b".repeat(32)),
        parents: vec![GitCommitId("c".repeat(40))],
        tree: JjTreeIdentity {
            terms: vec![GitTreeId("d".repeat(40))],
            labels: vec![],
        },
    }
}
pub fn binding() -> AttemptBinding {
    AttemptBinding {
        format_version: 1,
        project_id: "project".into(),
        owner: OwnerEpoch {
            owner: "engine".into(),
            epoch: 1,
        },
        run_id: "run".into(),
        run_epoch: 1,
        attempt_id: "attempt".into(),
        view_id: "view".into(),
        seed: reference(ObjectKind::Directory, "seed"),
        baseline: commit(),
        baseline_operation: JjOperationId("e".repeat(128)),
        projection_policy: reference(ObjectKind::Blob, "versioned-source-policy"),
        profile: reference(ObjectKind::Blob, "view-profile"),
    }
}
pub fn observation(binding: &AttemptBinding) -> LiveObservation {
    LiveObservation {
        view_id: binding.view_id.clone(),
        owner: binding.owner.clone(),
        cursor_namespace: "00aabd55-11fc-4a70-9d62-98407743e331".into(),
        request_nonce: "observation-nonce".into(),
        consistency: LiveObservationConsistency::LiveBestEffort,
        exclusion_policy_digest: "a".repeat(64),
        begin_generation: 1,
        end_generation: 2,
        directory: reference(ObjectKind::Directory, "live-directory"),
        projection_policy: binding.projection_policy.clone(),
        retention: "retained-observation".into(),
    }
}
pub fn proof(binding: AttemptBinding, tree: JjTreeIdentity) -> SealedSourceProof {
    SealedSourceProof {
        format_version: 1,
        binding,
        recovery_root: reference(ObjectKind::Checkpoint, "terminal-recovery"),
        source: reference(ObjectKind::SourceRoot, "versioned-source"),
        environment: Some(reference(
            ObjectKind::EnvironmentRoot,
            "prepared-environment",
        )),
        tree,
    }
}
#[cfg(feature = "jj")]
pub fn settings() -> jj_lib::settings::UserSettings {
    use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
    let mut config = StackedConfig::with_defaults();
    let mut layer = ConfigLayer::empty(ConfigSource::User);
    layer.set_value("user.name", "Rudder VCS Test").unwrap();
    layer
        .set_value("user.email", "vcs-test@invalid.example")
        .unwrap();
    config.add_layer(layer);
    jj_lib::settings::UserSettings::from_config(config).unwrap()
}
