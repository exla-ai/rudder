//! Early, fresh-process dispatch; PTY carries agent bytes, never a JSON spec.
use anyhow::{bail, Result};
#[cfg(target_os = "linux")]
use rudder_engine::execution::HelperServicesFactory;
#[cfg(target_os = "linux")]
fn services_factory(
    controller: Option<std::sync::Arc<rudder_engine::execution::handoff::ControllerProcessWitness>>,
) -> HelperServicesFactory {
    Box::new(move |spec| {
        // The engine invokes this only after single-thread namespace entry.
        // Host source uses the actual controller witness retained at handoff.
        if spec.profile.as_ref().is_some_and(|profile| {
            matches!(
                profile.workspace,
                rudder_engine::execution::ExecutionWorkspaceSelectionV4::RemoteSourceOnly { .. }
            )
        }) {
            if spec.format_version == 5 {
                let controller = controller.clone().ok_or_else(|| {
                    anyhow::anyhow!("host source requires managed controller custody")
                })?;
                crate::remote_workspace::services_host_source(spec, controller)
            } else {
                crate::remote_workspace::services(spec)
            }
        } else {
            crate::workspace_vcs::services(spec)
        }
    })
}
pub(crate) fn run(args: &[String]) -> Result<()> {
    if args.first().is_some_and(|a| a == "capabilities") {
        println!("{}", rudder_engine::execution::capabilities());
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        bail!("restricted execution requires a qualified Linux worker");
    }
    #[cfg(target_os = "linux")]
    {
        if args.len() == 1 && args[0] == "child" {
            rudder_engine::execution::linux::run_child()?;
            return Ok(());
        }
        if args.len() == 5
            && args[0] == "managed-helper"
            && args[1] == "--handoff"
            && args[3] == "--parent-pid"
        {
            let (spec, control) =
                rudder_engine::execution::handoff::receive_managed(&args[2], args[4].parse()?)?;
            let host_source = spec.format_version == 5;
            let factory = services_factory(Some(control.controller_process()));
            let outcome = if host_source {
                rudder_engine::execution::linux::run_helper_with_managed_host_source_services(
                    spec, factory, control,
                )?
            } else {
                rudder_engine::execution::linux::run_helper_with_managed_services(
                    spec, factory, control,
                )?
            };
            // Spec5 helper status proves finalization, while the record retains
            // the separate agent status. Historical reopen is not fresh proof.
            let exit = if host_source {
                if outcome.finalized().is_some() {
                    0
                } else {
                    125
                }
            } else {
                outcome.agent_exit_for_cli().unwrap_or(125)
            };
            std::process::exit(exit);
        }
        if args.len() != 5
            || args[0] != "helper"
            || args[1] != "--handoff"
            || args[3] != "--parent-pid"
        {
            bail!("execution helper requires its one-use parent descriptor handoff")
        }
        let spec = rudder_engine::execution::handoff::receive(&args[2], args[4].parse()?)?;
        let outcome = if spec.workspace_observation.is_some() {
            rudder_engine::execution::run_helper_with_services(spec, services_factory(None))?
        } else {
            rudder_engine::execution::run_helper(spec)?
        };
        // Protected execution record carries proof; never corrupt PTY bytes.
        std::process::exit(outcome.agent_exit_for_cli().unwrap_or(125));
    }
}
