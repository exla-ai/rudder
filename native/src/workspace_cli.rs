//! Dedicated managed-workspace CLI; helper dispatch occurs before App/threads.
use anyhow::{bail, Context, Result};
use rudder_engine::{
    protocol::{OwnerEpoch, FORMAT_VERSION},
    store::LocalStore,
    workspace::{CreateRequest, PrepareRequest, WorkspaceHelperSpec, WorkspaceManager},
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
        specification: PrepareRequest,
    },
    PrepareCheckpoint {
        checkpoint: rudder_engine::protocol::ObjectRef,
        backend: rudder_engine::workspace::WorkspaceBackend,
    },
    Create {
        specification: CreateRequest,
    },
    Descriptor {
        view_id: String,
        owner: OwnerEpoch,
    },
    ReassignOwner {
        view_id: String,
        current_owner: OwnerEpoch,
        next_owner: OwnerEpoch,
        operation_id: String,
    },
    Materialize {
        view_id: String,
        owner: OwnerEpoch,
    },
    CheckpointQuiesced {
        view_id: String,
        owner: OwnerEpoch,
        operation_id: String,
        seal: bool,
        writers_quiesced: bool,
        knowledge_cursor: u64,
    },
    Destroy {
        view_id: String,
        owner: OwnerEpoch,
        operation_id: String,
    },
    HelperSpec {
        view_id: String,
        owner: OwnerEpoch,
        command: Vec<String>,
        vcs_remint_command: Option<Vec<String>>,
        #[serde(default)]
        allow_unversioned: bool,
    },
    #[cfg(target_os = "linux")]
    TerminalCheckpoint {
        specification: rudder_engine::workspace::linux::TerminalCheckpointRequest,
    },
}
pub(crate) fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|arg| arg == "capabilities") {
        println!("{}", rudder_engine::workspace::capabilities());
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "helper") {
        #[cfg(target_os = "linux")]
        {
            let file = args
                .get(1)
                .context("workspace helper requires a spec file")?;
            let mut bytes = Vec::new();
            std::fs::File::open(file)?
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 64 * 1024 {
                bail!("helper specification exceeds 64 KiB");
            }
            let spec: WorkspaceHelperSpec = serde_json::from_slice(&bytes)?;
            let outcome = rudder_engine::workspace::linux::run_helper(spec)?;
            std::process::exit(outcome.exit_code);
        }
        #[cfg(not(target_os = "linux"))]
        bail!("managed namespace helper is supported only on qualified Linux hosts");
    }
    let mut bytes = Vec::new();
    io::stdin().take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        bail!("workspace request exceeds 1 MiB");
    }
    let request: Request = serde_json::from_slice(&bytes)?;
    if request.format_version != FORMAT_VERSION {
        bail!("unsupported workspace request format");
    }
    let manager =
        WorkspaceManager::open(request.manager_root, LocalStore::open(request.store_root)?)?;
    let result = match request.request {
        Operation::Prepare { specification } => {
            serde_json::to_value(manager.prepare(specification)?)?
        }
        Operation::PrepareCheckpoint {
            checkpoint,
            backend,
        } => serde_json::to_value(manager.prepare_checkpoint(&checkpoint, backend)?)?,
        Operation::Create { specification } => {
            serde_json::to_value(manager.create(specification)?)?
        }
        Operation::Descriptor { view_id, owner } => {
            serde_json::to_value(manager.descriptor(&view_id, &owner)?)?
        }
        Operation::ReassignOwner {
            view_id,
            current_owner,
            next_owner,
            operation_id,
        } => serde_json::to_value(manager.reassign_owner(
            &view_id,
            &current_owner,
            next_owner,
            &operation_id,
        )?)?,
        Operation::Materialize { view_id, owner } => {
            serde_json::to_value(manager.materialize(&view_id, &owner)?)?
        }
        Operation::CheckpointQuiesced {
            view_id,
            owner,
            operation_id,
            seal,
            writers_quiesced,
            knowledge_cursor,
        } => serde_json::to_value(manager.checkpoint_quiesced(
            &view_id,
            &owner,
            &operation_id,
            seal,
            writers_quiesced,
            knowledge_cursor,
        )?)?,
        Operation::Destroy {
            view_id,
            owner,
            operation_id,
        } => serde_json::to_value(manager.destroy(&view_id, &owner, &operation_id)?)?,
        Operation::HelperSpec {
            view_id,
            owner,
            command,
            vcs_remint_command,
            allow_unversioned,
        } => serde_json::to_value(manager.helper_spec(
            &view_id,
            &owner,
            command,
            vcs_remint_command,
            allow_unversioned,
        )?)?,
        #[cfg(target_os = "linux")]
        Operation::TerminalCheckpoint { specification } => serde_json::to_value(
            rudder_engine::workspace::linux::request_terminal_checkpoint(&manager, specification)?,
        )?,
    };
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({"format_version":1,"result":result}))?
    );
    Ok(())
}
