//! Early, fresh-process dispatch; PTY carries agent bytes, never a JSON spec.
use anyhow::{bail, Result};
#[cfg(target_os = "linux")]
use rudder_engine::execution::HelperServicesFactory;
#[cfg(target_os = "linux")]
fn services_factory() -> HelperServicesFactory {
    Box::new(|spec| {
        // The engine invokes this only after single-thread namespace entry.
        // No journal, caller-supplied author, or peer credential path is used.
        crate::workspace_vcs::services(spec)
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
        if args.len() != 5
            || args[0] != "helper"
            || args[1] != "--handoff"
            || args[3] != "--parent-pid"
        {
            bail!("execution helper requires its one-use parent descriptor handoff")
        }
        let spec = rudder_engine::execution::handoff::receive(&args[2], args[4].parse()?)?;
        let outcome = if spec.workspace_observation.is_some() {
            rudder_engine::execution::run_helper_with_services(spec, services_factory())?
        } else {
            rudder_engine::execution::run_helper(spec)?
        };
        // Protected execution record carries proof; never corrupt PTY bytes.
        std::process::exit(outcome.record.agent_exit.unwrap_or(125));
    }
}
