//! Bounded CLI bridge for the owned VFS backend; no App mutation.
use anyhow::{bail, Context, Result};
use rudder_engine::{
    protocol::{ObjectRef, OwnerEpoch},
    store::LocalStore,
    vfs::{VfsCheckpoint, VfsManager},
};
use serde::Deserialize;
use std::{
    io::{self, Read},
    path::PathBuf,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    format_version: u32,
    manager_root: PathBuf,
    store_root: PathBuf,
    request: Operation,
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Prepare {
        root: ObjectRef,
    },
    PrepareSnapshot {
        snapshot: VfsCheckpoint,
    },
    PrepareQuiescedPath {
        path: PathBuf,
        excluded_paths_hex: Vec<Vec<String>>,
        writers_quiesced: bool,
    },
    Fork {
        view_id: String,
        root: ObjectRef,
        owner: OwnerEpoch,
    },
    ForkSnapshot {
        view_id: String,
        snapshot: VfsCheckpoint,
        owner: OwnerEpoch,
    },
    Descriptor {
        view_id: String,
        owner: OwnerEpoch,
    },
    CheckpointUnmounted {
        view_id: String,
        owner: OwnerEpoch,
    },
    Seal {
        view_id: String,
        owner: OwnerEpoch,
    },
    Destroy {
        view_id: String,
        owner: OwnerEpoch,
    },
}
pub(crate) fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|s| s == "capabilities") {
        println!("{}", rudder_engine::vfs::capabilities());
        return Ok(());
    }
    let mut bytes = Vec::new();
    io::stdin().take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        bail!("VFS request exceeds1MiB")
    }
    let r: Request = serde_json::from_slice(&bytes)?;
    if r.format_version != 1 {
        bail!("unsupported VFS request")
    }
    let m = VfsManager::open(r.manager_root, LocalStore::open(r.store_root)?)?;
    let result = match r.request {
        Operation::Prepare { root } => {
            m.prepare(&root)?;
            serde_json::json!({"prepared":root})
        }
        Operation::PrepareSnapshot { snapshot } => {
            m.prepare_snapshot(&snapshot)?;
            serde_json::json!({"prepared":snapshot})
        }
        Operation::PrepareQuiescedPath {
            path,
            excluded_paths_hex,
            writers_quiesced,
        } => {
            if !writers_quiesced {
                bail!("path import requires all writers to be quiesced")
            };
            let excluded_paths = excluded_paths_hex
                .into_iter()
                .map(|p| p.iter().map(hex_decode).collect::<Result<Vec<_>>>())
                .collect::<Result<Vec<_>>>()?;
            serde_json::to_value(m.prepare_from_quiesced_path(
                &path,
                &rudder_engine::store::ImportPolicy { excluded_paths },
            )?)?
        }
        Operation::Fork {
            view_id,
            root,
            owner,
        } => serde_json::to_value(m.fork(&view_id, &root, owner)?.descriptor()?)?,
        Operation::ForkSnapshot {
            view_id,
            snapshot,
            owner,
        } => serde_json::to_value(m.fork_snapshot(&view_id, &snapshot, owner)?.descriptor()?)?,
        Operation::Descriptor { view_id, owner } => {
            serde_json::to_value(m.open_view(&view_id, &owner)?.descriptor()?)?
        }
        Operation::CheckpointUnmounted { view_id, owner } => serde_json::to_value(
            m.open_view(&view_id, &owner)?
                .checkpoint_snapshot_unmounted()
                .context("VFS checkpoint")?,
        )?,
        Operation::Seal { view_id, owner } => {
            serde_json::to_value(m.open_view(&view_id, &owner)?.seal()?)?
        }
        Operation::Destroy { view_id, owner } => {
            serde_json::to_value(m.open_view(&view_id, &owner)?.destroy()?)?
        }
    };
    println!(
        "{}",
        serde_json::json!({"format_version":1,"result":result})
    );
    Ok(())
}
fn hex_decode(s: &String) -> Result<Vec<u8>> {
    if s.len() % 2 != 0
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("invalid raw path component encoding")
    };
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}
