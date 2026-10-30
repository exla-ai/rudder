//! Opt-in mounted qualification tool; normal CI calls only --build-info.
#[cfg(target_os = "linux")]
#[path = "../vfs/qualification_driver.rs"]
mod qualification;

#[cfg(target_os = "linux")]
fn main() {
    qualification::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("mounted VFS qualification requires Linux");
    std::process::exit(1);
}
