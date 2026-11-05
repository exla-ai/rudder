use std::io::Write;

use jj_cli::cli_util::{CliRunner, CommandHelper};
use jj_cli::command_error::CommandError;
use jj_cli::ui::Ui;
use jj_lib::workspace::WorkingCopyFactories;

#[derive(clap::Parser, Clone, Debug)]
enum BridgeCommand {
    /// Show qualified adapter capabilities; this does not enable a VFS profile.
    RudderCapabilities,
    /// Add a workspace whose working copy is an overlay fork (rudder-overlay)
    /// with `--lower` (a directory already holding the revision's tree, e.g. a
    /// warm snapshot) as its read-only lower layer. No file is copied.
    #[command(hide = true)]
    RudderForkAdd(ForkAddArgs),
    /// Add a workspace that is an APFS copy-on-write clone of `--snapshot` (a
    /// warm snapshot: a local jj workspace) checked out at `-r`. The clone
    /// keeps the snapshot's recorded file states, so only files that differ
    /// between the snapshot's tree and the revision are written.
    #[command(hide = true)]
    RudderCloneAdd(CloneAddArgs),
    #[cfg(feature = "local-development")]
    /// Explicit development scanner profile, using private colocated metadata.
    RudderInitLocal,
}

#[derive(clap::Args, Clone, Debug)]
struct ForkAddArgs {
    destination: std::path::PathBuf,
    #[arg(long)]
    name: String,
    #[arg(long, short = 'r')]
    revision: String,
    #[arg(long)]
    lower: std::path::PathBuf,
    #[arg(long)]
    lowers_root: std::path::PathBuf,
    #[arg(long, short = 'm', default_value = "")]
    message: String,
}

#[derive(clap::Args, Clone, Debug)]
struct CloneAddArgs {
    destination: std::path::PathBuf,
    #[arg(long)]
    name: String,
    #[arg(long, short = 'r')]
    revision: String,
    #[arg(long)]
    snapshot: std::path::PathBuf,
    #[arg(long, short = 'm', default_value = "")]
    message: String,
}

#[cfg(not(target_os = "macos"))]
async fn clone_add(_ui: &mut Ui, _helper: &CommandHelper, _args: &CloneAddArgs) -> Result<(), CommandError> {
    Err(jj_cli::command_error::user_error("rudder-clone-add needs APFS clonefile (macOS)"))
}

#[cfg(target_os = "macos")]
async fn clone_add(ui: &mut Ui, helper: &CommandHelper, args: &CloneAddArgs) -> Result<(), CommandError> {
    use std::os::unix::ffi::OsStrExt as _;

    use jj_cli::command_error::{internal_error, user_error};
    use jj_lib::file_util;
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::local_working_copy::{LocalWorkingCopy, LockedLocalWorkingCopy};
    use jj_lib::ref_name::WorkspaceNameBuf;
    use jj_lib::repo::Repo as _;
    use jj_lib::working_copy::WorkingCopy as _;
    use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};
    let old = helper.workspace_helper(ui)?;
    let name = WorkspaceNameBuf::from(args.name.clone());
    if old.repo().view().get_wc_commit_id(&name).is_some() {
        return Err(user_error(format!("Workspace named '{}' already exists", args.name)));
    }
    let snapshot = helper.cwd().join(&args.snapshot);
    let wc_type = std::fs::read_to_string(snapshot.join(".jj/working_copy/type"))
        .map_err(|err| user_error(format!("{} is not a jj workspace: {err}", snapshot.display())))?;
    if wc_type.trim() != LocalWorkingCopy::name() {
        return Err(user_error(format!("{} has a '{}' working copy, not a local one", snapshot.display(), wc_type.trim())));
    }
    let parent_id = old
        .resolve_single_rev(ui, &jj_cli::cli_util::RevisionArg::from(args.revision.clone()))
        .await?
        .id()
        .clone();
    // One clonefile call clones the whole tree: data blocks are shared, and
    // mtime, size and mode are kept, so the recorded file states stay valid.
    // There is no byte-copy fallback; the caller makes a full checkout instead.
    let destination = helper.cwd().join(&args.destination);
    let from = std::ffi::CString::new(snapshot.as_os_str().as_bytes()).map_err(internal_error)?;
    let to = std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(internal_error)?;
    const CLONE_NOFOLLOW: u32 = 0x0001; // <sys/clonefile.h>
    if unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), CLONE_NOFOLLOW) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(user_error(format!("APFS clone of {} failed: {error}", snapshot.display())));
    }
    // Point .jj/repo at the repo from the new location, as `jj workspace add` writes it.
    let jj_dir = std::fs::canonicalize(destination.join(".jj")).map_err(internal_error)?;
    let repo_dir = std::fs::canonicalize(old.repo_path()).map_err(internal_error)?;
    let to_store = file_util::relative_path(&jj_dir, &repo_dir);
    let to_store = if to_store.is_relative() { file_util::slash_path(&to_store).into_owned() } else { to_store };
    std::fs::write(jj_dir.join("repo"), file_util::path_to_bytes(&to_store).map_err(internal_error)?)
        .map_err(internal_error)?;
    // Take over the snapshot's working-copy state (its recorded tree and file
    // states) under the new name. The snapshot's Watchman clock belongs to
    // another path; clearing it also rewrites the tree state on finish, so
    // every cloned file predates the state and is trusted by its recorded
    // stats instead of being hashed again.
    let root = std::fs::canonicalize(&destination).map_err(internal_error)?;
    let repo = old.repo().clone();
    let wc = LocalWorkingCopy::load(repo.store().clone(), root.clone(), jj_dir.join("working_copy"), old.settings())?;
    let mut locked = wc.start_mutation()?;
    locked.rename_workspace(name.clone());
    if let Some(local) = locked.downcast_mut::<LockedLocalWorkingCopy>() {
        local.reset_watchman()?;
    }
    let mut tx = jj_cli::cli_util::start_repo_transaction(&repo, &name, helper.string_args());
    let parent = repo.store().get_commit(&parent_id)?;
    let commit = tx
        .repo_mut()
        .new_commit(vec![parent_id], parent.tree())
        .set_description(&args.message)
        .write()
        .await?;
    tx.repo_mut().edit(name.clone(), &commit).await?;
    // Check out from the snapshot's recorded tree, writing only the files that
    // differ, before the workspace is published: a failure here leaves the
    // repo untouched.
    let stats = locked
        .check_out(&commit)
        .await
        .map_err(|err| jj_cli::command_error::internal_error_with_message("Failed to check out the clone", err))?;
    let repo = tx.commit(format!("create clone workspace {}", args.name)).await?;
    let finished = async {
        locked.finish(repo.op_id().clone()).await?;
        SimpleWorkspaceStore::load(&repo_dir)
            .and_then(|store| store.add(&name, &root))
            .map_err(internal_error)
    }
    .await;
    if let Err(error) = finished {
        // Unpublish the half-made workspace so a full checkout can take the name.
        let mut tx = repo.start_transaction();
        tx.repo_mut().remove_wc_commit(&name).await?;
        tx.commit(format!("forget failed clone workspace {}", args.name)).await?;
        return Err(error);
    }
    writeln!(
        ui.status(),
        "Cloned workspace {} at {}: {} files updated, {} added, {} removed",
        args.name,
        commit.id().hex(),
        stats.updated_files,
        stats.added_files,
        stats.removed_files
    )?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn fork_add(_ui: &mut Ui, _helper: &CommandHelper, _args: &ForkAddArgs) -> Result<(), CommandError> {
    Err(jj_cli::command_error::user_error("rudder-fork-add needs Linux OverlayFS"))
}

#[cfg(target_os = "linux")]
async fn fork_add(ui: &mut Ui, helper: &CommandHelper, args: &ForkAddArgs) -> Result<(), CommandError> {
    use jj_cli::command_error::{internal_error, user_error};
    use jj_lib::ref_name::WorkspaceNameBuf;
    use jj_lib::repo::Repo as _;
    use jj_lib::workspace::Workspace;
    use rudder_engine::jj_working_copy::{OverlayWorkingCopyFactory, link_lower};
    let old = helper.workspace_helper(ui)?;
    let name = WorkspaceNameBuf::from(args.name.clone());
    if old.repo().view().get_wc_commit_id(&name).is_some() {
        return Err(user_error(format!("Workspace named '{}' already exists", args.name)));
    }
    let destination = helper.cwd().join(&args.destination);
    std::fs::create_dir_all(&destination).map_err(internal_error)?;
    let parent_id = old
        .resolve_single_rev(ui, &jj_cli::cli_util::RevisionArg::from(args.revision.clone()))
        .await?
        .id()
        .clone();
    let parent = old.repo().store().get_commit(&parent_id)?;
    let tree = parent.tree();
    link_lower(&args.lowers_root, &tree, &args.lower).map_err(internal_error)?;
    let factory = OverlayWorkingCopyFactory { lowers_root: Some(args.lowers_root.clone()) };
    let (workspace, repo) =
        Workspace::init_workspace_with_existing_repo(&destination, old.repo_path(), old.repo(), &factory, name)
            .await?;
    let mut new_command = helper.for_workable_repo(ui, workspace, repo)?;
    let mut tx = new_command.start_transaction();
    let commit = tx
        .repo_mut()
        .new_commit(vec![parent_id], tree)
        .set_description(&args.message)
        .write()
        .await?;
    tx.edit(&commit)?;
    tx.finish(ui, format!("create overlay fork workspace {}", args.name)).await?;
    Ok(())
}

async fn command(
    ui: &mut Ui,
    helper: &CommandHelper,
    command: BridgeCommand,
) -> Result<(), CommandError> {
    match command {
        BridgeCommand::RudderForkAdd(args) => fork_add(ui, helper, &args).await?,
        BridgeCommand::RudderCloneAdd(args) => clone_add(ui, helper, &args).await?,
        BridgeCommand::RudderCapabilities => {
            let _ = helper;
            writeln!(
                ui.stdout_formatter(),
                "{}",
                serde_json::json!({"format_version":1,"jj_version":"0.40.0","vfs_default":false,"vfs_provider_required":true,"local_development":cfg!(feature="local-development"),"automatic_jj_replacement":false})
            )?;
        }
        #[cfg(feature = "local-development")]
        BridgeCommand::RudderInitLocal => {
            use jj_lib::{
                backend::Backend,
                git_backend::GitBackend,
                ref_name::WorkspaceName,
                repo::ReadonlyRepo,
                settings::UserSettings,
                signing::Signer,
                workspace::{Workspace, WorkspaceInitError},
            };
            use std::path::Path;
            let root = helper.cwd();
            let settings = helper.settings_for_new_workspace(ui, root)?.0;
            let backend = |settings: &UserSettings, store: &Path| {
                Ok(Box::new(GitBackend::init_colocated(
                    settings,
                    store,
                    Path::new("../../.."),
                )?) as Box<dyn Backend>)
            };
            Workspace::init_with_factories(
                &settings,
                root,
                &backend,
                Signer::from_settings(&settings).map_err(WorkspaceInitError::SignInit)?,
                ReadonlyRepo::default_op_store_initializer(),
                ReadonlyRepo::default_op_heads_store_initializer(),
                ReadonlyRepo::default_index_store_initializer(),
                ReadonlyRepo::default_submodule_store_initializer(),
                &rudder_vcs_bridge::local_development::LocalDevelopmentWorkingCopyFactory,
                WorkspaceName::DEFAULT.to_owned(),
            )
            .await?;
        }
    }
    Ok(())
}

#[derive(clap::Args, Clone, Debug)]
struct ManagedArgs {
    #[arg(long, hide = true)]
    rudder_command_id: String,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => code.into(),
        Err(error) => {
            eprintln!("Managed jj: {error:#}");
            std::process::ExitCode::from(1)
        }
    }
}

fn run() -> anyhow::Result<u8> {
    use rudder_vcs_bridge::command_transport;
    if let Some(config) = std::env::var_os("RUDDER_JJ_CLIENT_CONFIG") {
        anyhow::ensure!(
            std::env::var_os("RUDDER_JJ_HOST_CONTEXT").is_none(),
            "ambiguous managed CLI role"
        );
        return command_transport::run_guest_cli(
            std::path::Path::new(&config),
            &std::env::args().skip(1).collect::<Vec<_>>(),
        );
    }
    if let Some(context) = command_transport::host_context_from_environment()? {
        if matches!(
            context.command,
            command_transport::SemanticCommand::Edit { .. }
        ) {
            command_transport::run_semantic_edit(&context)?;
            return Ok(0);
        }
        let mut factories = WorkingCopyFactories::new();
        factories.insert(
            rudder_vcs_bridge::working_copy::TYPE_NAME.into(),
            Box::new(
                rudder_vcs_bridge::working_copy::RudderWorkingCopyFactory::new(
                    context.view()?,
                    None,
                ),
            ),
        );
        let expected = context.command_id.clone();
        return Ok(CliRunner::init()
            .name("rudder-jj")
            .version(command_transport::ADAPTER_VERSION)
            .add_extra_config(command_transport::fixed_layer(&context.service)?)
            .add_working_copy_factories(factories)
            .add_global_args::<ManagedArgs, _>(move |_ui, args| {
                if args.rudder_command_id != expected {
                    return Err(jj_cli::command_error::user_error(
                        "managed command identity changed",
                    ));
                }
                Ok(())
            })
            .run());
    }
    // Behaves like stock jj for normal repos; additionally loads repos on the
    // Rudder content store and rudder-overlay (fork) working copies.
    let mut factories = WorkingCopyFactories::new();
    #[cfg(target_os = "linux")]
    factories.insert(
        rudder_engine::jj_working_copy::OverlayWorkingCopy::name().to_owned(),
        Box::new(rudder_engine::jj_working_copy::OverlayWorkingCopyFactory { lowers_root: None }),
    );
    let mut store_factories = jj_lib::repo::StoreFactories::empty();
    rudder_engine::jj_store::register(&mut store_factories);
    #[cfg(feature = "local-development")]
    factories.insert(
        rudder_vcs_bridge::local_development::TYPE_NAME.into(),
        Box::new(rudder_vcs_bridge::local_development::LocalDevelopmentWorkingCopyFactory),
    );
    // Production RudderWorkingCopyFactory is injected by a qualified scoped
    // WorkspaceView integration, not silently replaced by the development WC.
    Ok(CliRunner::init()
        .name("rudder-jj")
        .version(command_transport::ADAPTER_VERSION)
        .add_store_factories(store_factories)
        .add_working_copy_factories(factories)
        .add_subcommand(command)
        .run())
}
