//! Explicit, no-key disposable transport qualifier; not a worker entrypoint.
fn main() {
    #[cfg(target_os = "linux")]
    let code = rudder_engine::execution::network::connected::qualification::run_cli(
        &std::env::args().skip(1).collect::<Vec<_>>(),
    );
    #[cfg(not(target_os = "linux"))]
    let code = {
        println!("{{\"kind\":\"connected-network-qualification-v1\",\"status\":\"unsupported\",\"hostFallback\":false}}");
        77
    };
    std::process::exit(code);
}
