use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    let protoc_path =
        protoc_bin_vendored::protoc_bin_path().expect("failed to get vendored protoc binary");
    unsafe {
        std::env::set_var("PROTOC", &protoc_path);
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(
            &["proto/tno/v1/types.proto", "proto/tno/v1/api.proto"],
            &["proto"],
        )?;

    Ok(())
}
