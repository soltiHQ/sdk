#[cfg(any(feature = "grpc", feature = "http"))]
use protoc_bin_vendored::protoc_bin_path;
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
#[cfg(any(feature = "grpc", feature = "http"))]
use version::DISCOVERY_GRPC_SERVICE;
use version::DISCOVERY_PROTOCOL_VERSION;

#[cfg(any(feature = "grpc", feature = "http"))]
const PROTO_INCLUDE_ROOT: &str = "proto";
#[cfg(any(feature = "grpc", feature = "http"))]
const PROTO_SOURCE_ROOT: &str = "proto/solti";

#[cfg(not(any(feature = "grpc", feature = "http")))]
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/version.rs");
    println!("cargo:rerun-if-changed=proto");
    export_contract_identity();
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/version.rs");
    println!("cargo:rerun-if-changed={PROTO_INCLUDE_ROOT}");
    export_contract_identity();

    let protoc_path = protoc_bin_path().expect("failed to get vendored protoc binary");
    unsafe {
        env::set_var("PROTOC", &protoc_path);
    }

    let grpc = env::var_os("CARGO_FEATURE_GRPC").is_some();
    let protos = collect_proto_files(Path::new(PROTO_SOURCE_ROOT))?;
    if protos.is_empty() {
        return Err(format!("no .proto files found under '{PROTO_SOURCE_ROOT}/'").into());
    }
    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let out_dir: PathBuf = env::var("OUT_DIR")?.into();
    let descriptor_path = out_dir.join("solti_discover_descriptor.bin");

    tonic_prost_build::configure()
        .build_server(false)
        .build_client(grpc)
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&protos, &[PathBuf::from(PROTO_INCLUDE_ROOT)])?;

    let descriptor_set = std::fs::read(&descriptor_path)?;
    let pool = prost_reflect::DescriptorPool::decode(descriptor_set.as_slice())?;
    if pool.get_service_by_name(DISCOVERY_GRPC_SERVICE).is_none() {
        return Err(
            format!("protobuf descriptor has no service '{DISCOVERY_GRPC_SERVICE}'").into(),
        );
    }

    #[cfg(feature = "http")]
    {
        let discover_package = format!(".{DISCOVERY_GRPC_PACKAGE}");
        let packages = [".solti.agent.v1", discover_package.as_str()];
        pbjson_build::Builder::new()
            .register_descriptors(&descriptor_set)?
            .build(&packages)?;
    }

    Ok(())
}

fn export_contract_identity() {
    println!("cargo:rustc-env=SOLTI_DISCOVERY_PROTOCOL_MAJOR={DISCOVERY_PROTOCOL_VERSION}");
}

/// Recursively collect every `*.proto` file under `root`.
#[cfg(any(feature = "grpc", feature = "http"))]
fn collect_proto_files(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && path.extension().is_some_and(|e| e == "proto") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}
