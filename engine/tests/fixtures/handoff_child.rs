//! Fresh child for the private one-use handoff protocol's Linux tests.
#[cfg(target_os = "linux")]
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let spec =
        rudder_engine::execution::handoff::receive(&args[1], args[2].parse().unwrap()).unwrap();
    let fixture = b"known-fixture-secret-never-argv";
    assert_eq!(spec.environment["CANARY_SECRET"].as_bytes(), fixture);
    for path in ["/proc/self/cmdline", "/proc/self/environ"] {
        assert!(!std::fs::read(path)
            .unwrap()
            .windows(fixture.len())
            .any(|bytes| bytes == fixture));
    }
    println!("handoff-ok");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("execution handoff test requires Linux");
    std::process::exit(1);
}
