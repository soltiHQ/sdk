#[cfg(any(feature = "grpc", feature = "http"))]
use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
};

#[allow(dead_code)]
#[path = "src/version.rs"]
mod version;
#[cfg(feature = "http")]
use version::DISCOVERY_GRPC_PACKAGE;
use version::DISCOVERY_PROTOCOL_VERSION;

#[cfg(any(feature = "grpc", feature = "http"))]
const PROTO_INCLUDE_ROOT: &str = "proto";
#[cfg(any(feature = "grpc", feature = "http"))]
const PROTO_SOURCE_ROOT: &str = "proto/solti";

#[cfg(not(any(feature = "grpc", feature = "http")))]
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/version.rs");
    export_contract_identity();
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/version.rs");
    println!("cargo:rerun-if-changed={PROTO_INCLUDE_ROOT}");
    export_contract_identity();

    let source_root = Path::new(PROTO_SOURCE_ROOT);
    if !source_root.is_dir() {
        return Err(format!(
            "expected proto directory '{}'; run `task proto/vendor`",
            source_root.display(),
        )
        .into());
    }

    let protos = collect_proto_files(source_root)?;
    if protos.is_empty() {
        return Err(format!("no .proto files found under '{PROTO_SOURCE_ROOT}'").into());
    }
    for proto in &protos {
        println!("cargo:rerun-if-changed={}", proto.display());
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("solti_discover_descriptor.bin");
    let grpc = env::var_os("CARGO_FEATURE_GRPC").is_some();
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);

    tonic_prost_build::configure()
        .build_server(false)
        .build_client(grpc)
        .emit_rerun_if_changed(false)
        .out_dir(&out_dir)
        .file_descriptor_set_path(&descriptor_path)
        .compile_with_config(prost, &protos, &[PathBuf::from(PROTO_INCLUDE_ROOT)])?;

    #[cfg(feature = "http")]
    {
        let descriptor_set = std::fs::read(&descriptor_path)?;
        let discover_package = format!(".{DISCOVERY_GRPC_PACKAGE}");
        pbjson_build::Builder::new()
            .out_dir(&out_dir)
            .register_descriptors(&descriptor_set)?
            .build(&[".solti.agent.v1", discover_package.as_str()])?;
    }

    Ok(())
}

fn export_contract_identity() {
    println!("cargo:rustc-env=SOLTI_DISCOVERY_PROTOCOL_MAJOR={DISCOVERY_PROTOCOL_VERSION}");
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn collect_proto_files(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut protos = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "proto")
            {
                protos.push(path);
            }
        }
    }
    protos.sort();
    Ok(protos)
}
