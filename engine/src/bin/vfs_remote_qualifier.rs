//! Disposable Linux qualification, never an installed agent entrypoint.
#[cfg(target_os = "linux")]
#[path = "../vfs/remote_qualification.rs"]
mod qualification;

#[cfg(target_os = "linux")]
fn main() {
    qualification::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("remote VFS qualification requires Linux");
    std::process::exit(1);
}
