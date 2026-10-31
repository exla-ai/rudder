//! Disposable source-only qualification entrypoint, never a worker default.
fn main() {
    #[cfg(target_os = "linux")]
    std::process::exit(rudder_engine::execution::remote_qualification::run_cli());

    #[cfg(not(target_os = "linux"))]
    {
        println!("{{\"kind\":\"remote-execution-qualification\",\"status\":\"unsupported\",\"reason\":\"LinuxRequired\",\"hostFallback\":false}}");
        std::process::exit(77);
    }
}
