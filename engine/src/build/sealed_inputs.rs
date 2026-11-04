//! Per-build retention, held across queueing, frontend execution and publication.
//! Only the existing checkpoint/runnable qualifiers can produce the input proof.
use super::*;
use crate::workspace::{self, ProjectionBudget};
use std::time::Duration;

pub(super) struct SealedBuildInputs { store: LocalStore, pin: String, settled: bool }
impl SealedBuildInputs {
    pub(super) fn acquire(index: &BuildIndex, input: &BuildInputView, prepared: &PreparedEnvironment) -> Result<Self> {
        let store = index.store();
        let pin = format!("build/input/{}", uuid::Uuid::new_v4());
        let lease = store.begin_staging(Duration::from_secs(300))?;
        // Transfer from the prepared receipt rather than rehashing its SDK.
        // New source/profile/topology/checkpoint data is fully verified before
        // this attempt can use it. GC sees the new pin before execution starts.
        let published = store.publish_extending(&lease.id, &pin, input.checkpoint(), std::slice::from_ref(&prepared.receipt));
        if published.is_err() { let _ = store.abandon_staging(&lease.id); }
        published?;
        let owner = Self { store: store.clone(), pin, settled: true };
        owner.validate(input)?;
        Ok(owner)
    }
    pub(super) fn pin_data(&self) -> &str { &self.pin }
    pub(super) fn mark_inflight(&mut self) { self.settled = false; }
    pub(super) fn mark_settled(&mut self) { self.settled = true; }
    pub(super) fn validate(&self, expected: &BuildInputView) -> Result<()> {
        let actual = match expected.checkpoint().kind {
            ObjectKind::Checkpoint => workspace::build_input_view(&self.store, expected.checkpoint())?,
            ObjectKind::RunnableRoot => {
                let root = crate::snapshot::decode_runnable(&self.store.read_object(expected.checkpoint())?)?;
                let source = crate::snapshot::decode_source(&self.store.read_object(&root.source)?)?;
                let qualified = workspace::runnable_build_input_view(&self.store, expected.checkpoint(), &source.projection_policy, &ProjectionBudget::default())?;
                qualified.input().clone()
            }
            _ => anyhow::bail!("managed build input requires a sealed checkpoint or runnable root"),
        };
        ensure!(&actual == expected, "sealed build input differs from current verified checkpoint");
        Ok(())
    }
}
impl Drop for SealedBuildInputs {
    // Unknown shutdown retains the original pin recorded in the inflight
    // marker. Recovery must reconcile that original attempt before release.
    fn drop(&mut self) { if self.settled { let _ = self.store.unpin(&self.pin); } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::{CheckpointConsistency, Durability}, store::{CheckpointV2Manifest,
        DirectoryEntry, DirectoryManifest, EntryNode, SourceManifest}, workspace::WorkspaceProfile};
    use std::fs;

    fn fixture() -> (tempfile::TempDir, BuildIndex, PreparedEnvironment, BuildInputView) {
        let temp=tempfile::tempdir().unwrap();
        let store=LocalStore::open(temp.path().join("cas")).unwrap();
        let compiler=store.put_blob(b"pinned compiler identity").unwrap();
        let directory=store.put_directory(DirectoryManifest { format_version:1,mode:0o755,
            entries:vec![DirectoryEntry::new(b"compiler",EntryNode::File { object:compiler.clone(),mode:0o755 }).unwrap()] }).unwrap();
        let spec=PreparationSpec { format_version:1,platform:PlatformIdentity { os:"linux".into(),architecture:std::env::consts::ARCH.into(),abi:"test".into(),properties:BTreeMap::new() },
            base_runtime:directory.clone(),installer_inputs:BTreeMap::new(),installer_commands:vec![],installer_environment:BTreeMap::new(),
            toolchains:BTreeMap::from([("compiler".into(),ToolchainIdentity { version:"1".into(),closure:directory.clone(),executable:compiler,executable_path:"compiler".into(),configuration:BTreeMap::new() })]),policy_version:"test".into(),effects:BTreeSet::new() };
        let index=BuildIndex::open(temp.path().join("index"),store.clone()).unwrap();
        let prepared=index.publish_prepared(&spec,&directory).unwrap();
        let source=store.put_metadata(ObjectKind::SourceRoot,&SourceManifest { format_version:1,directory:directory.clone(),parents:vec![],consistency:CheckpointConsistency::Quiesced }).unwrap();
        let profile=store.put_blob(&canonical_json(&WorkspaceProfile::source_only()).unwrap()).unwrap();
        let checkpoint=store.put_checkpoint_v2(&CheckpointV2Manifest { format_version:2,source,environment:Some(prepared.root.clone()),view_root:directory,profile,
            parents:vec![],knowledge_cursor:0,consistency:CheckpointConsistency::Quiesced,durability:Durability::Local }).unwrap();
        let input=workspace::build_input_view(&store,&checkpoint).unwrap();
        (temp,index,prepared,input)
    }
    #[test]
    fn retained_input_survives_gc_and_releases_only_after_settlement() {
        let (_temp,index,prepared,input)=fixture();
        let mut owner=SealedBuildInputs::acquire(&index,&input,&prepared).unwrap();
        let pin=owner.pin_data().to_owned();
        assert_eq!(index.store().retention().unwrap().pins.get(&pin),Some(input.checkpoint()));
        index.store().gc(Duration::ZERO).unwrap();
        owner.validate(&input).unwrap();
        owner.mark_inflight(); drop(owner);
        assert_eq!(index.store().retention().unwrap().pins.get(&pin),Some(input.checkpoint()),"unresolved original custody must survive its stack frame");
        index.store().unpin(&pin).unwrap();
        let mut owner=SealedBuildInputs::acquire(&index,&input,&prepared).unwrap();
        let settled_pin=owner.pin_data().to_owned();
        owner.mark_inflight(); owner.mark_settled(); drop(owner);
        assert!(!index.store().retention().unwrap().pins.contains_key(&settled_pin));
    }
    #[test]
    fn production_caller_rejects_stale_proof_before_creating_build_scratch() {
        let (temp,index,prepared,mut input)=fixture();
        input.profile=index.store().put_blob(b"stale proof identity").unwrap();
        let adapter=Buck2Adapter::new("/usr/bin/true").unwrap();
        let mut profile=BuildProfile::from_toml(include_str!("../../../examples/build-profile/rudder.toml")).unwrap();
        profile.platform.architecture=std::env::consts::ARCH.into();
        use sha2::{Digest,Sha256};
        profile.buck2.executable_sha256=hex::encode(Sha256::digest(fs::read("/usr/bin/true").unwrap()));
        let scratch=temp.path().join("must-not-execute");
        let error=adapter.build(&index,&profile,&input,&prepared,&scratch).unwrap_err();
        assert!(error.to_string().contains("sealed build input differs"),"{error:#}");
        assert!(!scratch.exists());
    }
    #[test]
    fn poisoned_checkpoint_bytes_refuse_admission_without_a_pin_leak() {
        let (_temp,index,prepared,input)=fixture();
        let path=index.store().object_path(input.checkpoint()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path,fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(path,vec![b'?';input.checkpoint().size_bytes as usize]).unwrap();
        let before=index.store().retention().unwrap().pins;
        assert!(SealedBuildInputs::acquire(&index,&input,&prepared).is_err());
        assert_eq!(before,index.store().retention().unwrap().pins);
    }
}
