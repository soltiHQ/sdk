use std::{env, error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto/solti/v1/types.proto");
    println!("cargo:rerun-if-changed=proto/solti/v1/api.proto");

    let protoc_path =
        protoc_bin_vendored::protoc_bin_path().expect("failed to get vendored protoc binary");
    // SAFETY: build.rs runs single-threaded at compile time before any other
    // crate code observes the environment; no data race is possible.
    unsafe {
        env::set_var("PROTOC", &protoc_path);
    }

    let grpc = env::var_os("CARGO_FEATURE_GRPC").is_some();
    let http = env::var_os("CARGO_FEATURE_HTTP").is_some();
    if !grpc && !http {
        return Ok(());
    }

    let out_dir: PathBuf = env::var("OUT_DIR")?.into();
    let descriptor_path = out_dir.join("solti_api_descriptor.bin");

    tonic_prost_build::configure()
        .build_server(grpc)
        .build_client(grpc)
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(
            &["proto/solti/v1/types.proto", "proto/solti/v1/api.proto"],
            &["proto"],
        )?;

    if http {
        let descriptor_set = std::fs::read(&descriptor_path)?;
        pbjson_build::Builder::new()
            // Emit default values (empty arrays, 0, false, "") instead of omitting
            // them: REST clients expect stable field presence, not canonical
            // proto-JSON sparseness. Optional (proto3 `optional`) fields still
            // serialize to `null` when absent.
            .emit_fields()
            .register_descriptors(&descriptor_set)?
            .build(&[".solti.v1"])?;
    }

    Ok(())
}
