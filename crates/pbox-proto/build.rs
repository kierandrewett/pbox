fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/pbox/agent/v1/agent.proto");
    tonic_prost_build::configure()
        .compile_protos(&["../../proto/pbox/agent/v1/agent.proto"], &["../../proto"])?;
    Ok(())
}
