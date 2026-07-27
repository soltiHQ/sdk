#[cfg(any(feature = "grpc", feature = "http"))]
use protoc_bin_vendored::protoc_bin_path;
#[cfg(any(feature = "grpc", feature = "http"))]
use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
};

#[cfg(any(feature = "grpc", feature = "http"))]
const PROTO_ROOT: &str = "proto";
#[cfg(feature = "http")]
const PROTO_PACKAGES: &[&str] = &[".solti.agent.v1", ".solti.discover.v1"];

#[cfg(not(any(feature = "grpc", feature = "http")))]
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto");
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={PROTO_ROOT}");

    let protoc_path = protoc_bin_path().expect("failed to get vendored protoc binary");
    unsafe {
        env::set_var("PROTOC", &protoc_path);
    }

    let grpc = env::var_os("CARGO_FEATURE_GRPC").is_some();
    let protos = collect_proto_files(Path::new(PROTO_ROOT))?;
    if protos.is_empty() {
        return Err(format!("no .proto files found under '{PROTO_ROOT}/'").into());
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
        .compile_protos(&protos, &[PathBuf::from(PROTO_ROOT)])?;

    #[cfg(feature = "http")]
    {
        let descriptor_set = std::fs::read(&descriptor_path)?;
        pbjson_build::Builder::new()
            .register_descriptors(&descriptor_set)?
            .build(PROTO_PACKAGES)?;
    }

    Ok(())
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
