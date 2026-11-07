fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/autarqia/worker/v1/worker.proto");
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &["../../proto/autarqia/worker/v1/worker.proto"],
            &["../../proto"],
        )?;
    Ok(())
}
