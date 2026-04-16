use protoc_bin_vendored::protoc_bin_path;
use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
};

const PROTO_ROOT: &str = "proto";
const PROTO_PACKAGE: &str = ".solti.discover.v1";

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={PROTO_ROOT}");

    let protoc_path = protoc_bin_path().expect("failed to get vendored protoc binary");
    unsafe {
        env::set_var("PROTOC", &protoc_path);
    }

    let grpc = env::var_os("CARGO_FEATURE_GRPC").is_some();
    let http = env::var_os("CARGO_FEATURE_HTTP").is_some();
    if !grpc && !http {
        return Ok(());
    }

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
        .build_server(grpc)
        .build_client(grpc)
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&protos, &[PathBuf::from(PROTO_ROOT)])?;

    if http {
        let descriptor_set = std::fs::read(&descriptor_path)?;
        pbjson_build::Builder::new()
            .register_descriptors(&descriptor_set)?
            .build(&[PROTO_PACKAGE])?;
    }

    Ok(())
}

/// Recursively collect every `*.proto` file under `root`.
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
