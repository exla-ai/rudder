//! Image-owned root launcher. Actual worker ACK gates startup; the bounded
//! observation line is best-effort DATA and never gates machine supervision.
//! No user argv/environment/config forwarding.
#[cfg(target_os="linux")]
fn main() {
    std::panic::set_hook(Box::new(|_|eprintln!("MACHINE_ROOT_INTERNAL_FAULT")));
    if rudder_native::root_storage_bootstrap::run_machine_root().is_err() {
        eprintln!("MACHINE_ROOT_BOOTSTRAP_REJECTED");std::process::exit(78);
    }
}
#[cfg(not(target_os="linux"))]
fn main() {eprintln!("MACHINE_ROOT_REQUIRES_LINUX");std::process::exit(78);}
