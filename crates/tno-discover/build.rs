use protoc_bin_vendored::protoc_bin_path;
use std::{env, error::Error};

fn main() -> Result<(), Box<dyn Error>> {
    let protoc_path = protoc_bin_path().expect("failed to get vendored protoc binary");

    unsafe {
        env::set_var("PROTOC", &protoc_path);
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(&["proto/v1/sync.proto"], &["proto"])?;
    Ok(())
}
