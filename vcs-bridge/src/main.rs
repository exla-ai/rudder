use std::io::Write;

use jj_cli::cli_util::{CliRunner, CommandHelper};
use jj_cli::command_error::CommandError;
use jj_cli::ui::Ui;
use jj_lib::workspace::WorkingCopyFactories;

#[derive(clap::Parser, Clone, Debug)]
enum BridgeCommand {
    /// Show qualified adapter capabilities; this does not enable a VFS profile.
    RudderCapabilities,
    #[cfg(feature = "local-development")]
    /// Explicit development scanner profile, using private colocated metadata.
    RudderInitLocal,
}

async fn command(
    ui: &mut Ui,
    helper: &CommandHelper,
    command: BridgeCommand,
) -> Result<(), CommandError> {
    match command {
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
    #[allow(unused_mut)]
    let mut factories = WorkingCopyFactories::new();
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
        .add_working_copy_factories(factories)
        .add_subcommand(command)
        .run())
}
