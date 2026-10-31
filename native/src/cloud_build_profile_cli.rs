//! Local build DATA adapter. No launch, source-issuance or scheduler authority.
//! The caller must already own a retained, quiesced RunnableRoot and supply its
//! trusted projection policy. Peer-provided policy JSON is not launch proof.

use anyhow::{bail, ensure, Context, Result};
use rudder_engine::{
    build::{Buck2Adapter, BuildProfile},
    content,
    directory::EntryNode,
    protocol::{ObjectKind, ObjectRef},
    snapshot,
    store::LocalStore,
    workspace::{runnable_build_input_view, ProjectionBudget},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_REQUEST: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    format_version: u32,
    store: PathBuf,
    index: PathBuf,
    scratch: PathBuf,
    buck2: PathBuf,
    runnable: ObjectRef,
    expected_projection_policy: ObjectRef,
}

fn parse(raw: &[u8]) -> Result<Request> {
    ensure!(
        raw.len() <= MAX_REQUEST,
        "build-profile request exceeds 64 KiB"
    );
    let request: Request = serde_json::from_slice(raw)?;
    ensure!(
        request.format_version == 1,
        "unsupported build-profile request"
    );
    request.runnable.validate()?;
    request.expected_projection_policy.validate()?;
    ensure!(
        request.runnable.kind == ObjectKind::RunnableRoot,
        "build input requires retained RunnableRoot"
    );
    ensure!(
        request.expected_projection_policy.kind == ObjectKind::Blob,
        "expected projection policy must be a Blob"
    );
    for path in [
        &request.store,
        &request.index,
        &request.scratch,
        &request.buck2,
    ] {
        ensure!(path.is_absolute(), "build-profile paths must be absolute");
    }
    Ok(request)
}

fn existing_directory(path: &Path) -> Result<PathBuf> {
    ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "build-profile path must be an existing real directory"
    );
    Ok(path.canonicalize()?)
}

fn source_profile(store: &LocalStore, source: &ObjectRef) -> Result<(ObjectRef, BuildProfile)> {
    let source = snapshot::decode_source(&store.read_object(source)?)?;
    let entry = rudder_engine::tree::lookup(store, &source.directory, b"rudder.toml")?
        .context("retained source has no root rudder.toml")?;
    let EntryNode::File { object, .. } = entry.node else {
        bail!("root rudder.toml must be an ordinary source file");
    };
    let info = content::file_info(store, &object)?;
    ensure!(
        info.logical_size <= MAX_REQUEST as u64,
        "rudder.toml exceeds 64 KiB"
    );
    let bytes = content::read_file_range(store, &object, 0, info.logical_size as usize)?;
    let profile = BuildProfile::from_toml(std::str::from_utf8(&bytes)?)?;
    Ok((object, profile))
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    ensure!(
        args.is_empty(),
        "fabric build-profile accepts only JSON on stdin"
    );
    let mut raw = Vec::new();
    io::stdin()
        .take(MAX_REQUEST as u64 + 1)
        .read_to_end(&mut raw)?;
    let request = parse(&raw)?;
    ensure!(
        cfg!(target_os = "linux"),
        "managed build-profile requires Linux"
    );
    let store_path = existing_directory(&request.store)?;
    let index_path = existing_directory(&request.index)?;
    let scratch = existing_directory(&request.scratch)?;
    ensure!(
        store_path.join("retention.json").is_file(),
        "build-profile requires an existing retained CAS"
    );
    ensure!(
        index_path.join("index.sqlite").is_file(),
        "build-profile requires an existing preparation index"
    );
    ensure!(
        !scratch.starts_with(&store_path) && !scratch.starts_with(&index_path),
        "build scratch must be outside CAS and index directories"
    );
    // Never bootstrap CAS or manufacture a preparation/source root here.
    let store = LocalStore::open(store_path)?;
    let input = runnable_build_input_view(
        &store,
        &request.runnable,
        &request.expected_projection_policy,
        &ProjectionBudget::default(),
    )?;
    let (profile_ref, profile) = source_profile(&store, input.input().source_root())?;
    let index = rudder_engine::build::BuildIndex::open(index_path, store.clone())?;
    let environment = input
        .input()
        .base_environment()
        .context("build needs a prepared environment")?;
    let prepared = index
        .prepared_root(environment)?
        .context("environment has no retained preparation authority")?;
    let adapter = Buck2Adapter::new(&request.buck2)?;
    // Each invocation owns unique mutable scratch, even for identical inputs.
    let workspace = scratch.join(format!("build-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&workspace)?;
    let built = adapter.build(&index, &profile, input.input(), &prepared, &workspace);
    // Stop the private daemon on both success and failure. Failed/uncertain
    // scratch remains recoverable; this adapter never deletes a live workspace.
    let stopped = if workspace.join(".rudder-managed-build.json").is_file() {
        adapter.stop(&workspace)
    } else {
        Ok(())
    };
    let build = built?;
    stopped?;
    // Buck2 always validates its graph/REAPI identity. record_build and its run
    // publication retain the verified output closure transactionally. Scrub it
    // again before returning this existing receipt as successful build DATA.
    let objects = store.closure(&build.stored.outputs)?;
    store.closure(&build.run_receipt)?;
    let response = json!({
        "format_version": 1, "kind": "managed-build-data", "runnable": request.runnable,
        "profile": profile_ref, "workspace": workspace, "target_count": profile.targets.len(),
        "outputs_verified": true, "output_object_count": objects.len(), "build": build,
    });
    input.release()?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> serde_json::Value {
        json!({"format_version":1,"store":"/store","index":"/index","scratch":"/scratch","buck2":"/buck2",
            "runnable":ObjectRef::from_bytes(ObjectKind::RunnableRoot,b"runnable"),
            "expected_projection_policy":ObjectRef::from_bytes(ObjectKind::Blob,b"policy")})
    }

    #[test]
    fn build_profile_request_cannot_assert_build_or_preparation_authority() {
        assert!(parse(&serde_json::to_vec(&request()).unwrap()).is_ok());
        for field in [
            "input_view",
            "prepared",
            "profile",
            "source_root",
            "effects",
        ] {
            let mut value = request();
            value[field] = json!({});
            assert!(parse(&serde_json::to_vec(&value).unwrap()).is_err());
        }
        let mut value = request();
        value["runnable"] = json!(ObjectRef::from_bytes(ObjectKind::SourceRoot, b"source"));
        assert!(parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn build_profile_request_is_bounded_and_requires_explicit_paths() {
        assert!(parse(&vec![b' '; MAX_REQUEST + 1]).is_err());
        let mut value = request();
        value["scratch"] = json!("relative");
        assert!(parse(&serde_json::to_vec(&value).unwrap()).is_err());
        value = request();
        value["format_version"] = json!(2);
        assert!(parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn build_profile_is_read_from_frozen_source_and_rejects_symlink_profiles() {
        let root =
            std::env::temp_dir().join(format!("rudder-build-profile-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let result = (|| -> Result<()> {
            let store = LocalStore::open(root.join("cas"))?;
            let lease = store.begin_staging(std::time::Duration::from_secs(30))?;
            let source_path = root.join("source");
            fs::create_dir(&source_path)?;
            let text = include_str!("../../examples/build-profile/rudder.toml");
            fs::write(source_path.join("rudder.toml"), text)?;
            let directory = store.import_tree(
                &source_path,
                rudder_engine::protocol::CheckpointConsistency::Quiesced,
            )?;
            let source = snapshot::stage(
                &store,
                &lease.id,
                ObjectKind::SourceRoot,
                &snapshot::SourceV2 {
                    format_version: 2,
                    consistency: rudder_engine::protocol::CheckpointConsistency::Quiesced,
                    directory,
                    projection_policy: store.put_blob(b"{}")?,
                },
            )?;
            fs::write(
                source_path.join("rudder.toml"),
                "invalid changed host profile",
            )?;
            let (_, profile) = source_profile(&store, &source)?;
            assert_eq!(profile, BuildProfile::from_toml(text)?);
            #[cfg(unix)]
            {
                fs::remove_file(source_path.join("rudder.toml"))?;
                std::os::unix::fs::symlink(
                    "/untrusted/host/profile",
                    source_path.join("rudder.toml"),
                )?;
                let directory = store.import_tree(
                    &source_path,
                    rudder_engine::protocol::CheckpointConsistency::Quiesced,
                )?;
                let source = snapshot::stage(
                    &store,
                    &lease.id,
                    ObjectKind::SourceRoot,
                    &snapshot::SourceV2 {
                        format_version: 2,
                        consistency: rudder_engine::protocol::CheckpointConsistency::Quiesced,
                        directory,
                        projection_policy: store.put_blob(b"{}")?,
                    },
                )?;
                assert!(source_profile(&store, &source).is_err());
            }
            store.abandon_staging(&lease.id)?;
            Ok(())
        })();
        fs::remove_dir_all(root).unwrap();
        result.unwrap();
    }
}
