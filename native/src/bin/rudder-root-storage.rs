//! Image-owned root entry only. No user argv/env/path forwarding.
#[cfg(target_os = "linux")]
fn main() {
    std::panic::set_hook(Box::new(|_| eprintln!("ROOT_STORAGE_INTERNAL_FAULT")));
    if rudder_native::root_storage_service::run_inherited_root_service().is_err() {
        eprintln!("ROOT_STORAGE_BOOTSTRAP_REJECTED");
        std::process::exit(78);
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ROOT_STORAGE_REQUIRES_LINUX");
    std::process::exit(78);
}
