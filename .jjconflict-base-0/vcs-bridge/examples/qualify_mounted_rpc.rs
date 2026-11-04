//! Explicit disposable-worker qualification, never a production launch path.
//! Build with `--features cli --example qualify_mounted_rpc`.
#[cfg(all(target_os = "linux", feature = "cli"))]
#[path = "mounted_rpc/driver.rs"]
mod driver;

fn main() {
    #[cfg(all(target_os = "linux", feature = "cli"))]
    if let Err(error) = driver::main() {
        eprintln!("mounted VCS qualification: {error:#}");
        std::process::exit(1);
    }
    #[cfg(not(all(target_os = "linux", feature = "cli")))]
    {
        eprintln!("requires an explicitly owned Linux worker and feature cli");
        std::process::exit(1);
    }
}
