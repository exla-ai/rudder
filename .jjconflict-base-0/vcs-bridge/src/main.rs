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
