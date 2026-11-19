//! Compiles `crates/distlock-proto/proto/distlock/v1/distlock.proto` with `protox` (a pure-Rust protobuf compiler),
//! so building this workspace does not require a system `protoc`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(["distlock/v1/distlock.proto"], ["proto"])?;
    prost_build::Config::new().compile_fds(fds)?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
