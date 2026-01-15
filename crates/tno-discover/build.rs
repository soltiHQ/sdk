use std::{error::Error, env};
use protoc_bin_vendored::protoc_bin_path;

fn main() -> Result<(), Box<dyn Error>> {
    let protoc_path = protoc_bin_path().expect("failed to get vendored protoc binary");
    
    unsafe {
        env::set_var("PROTOC", &protoc_path);
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &["proto/v1/sync.proto"],
            &["proto"],
        )?;
    Ok(())
}