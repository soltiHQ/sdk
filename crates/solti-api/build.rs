use std::error::Error;

#[cfg(feature = "grpc")]
use std::{
    env,
    path::{Path, PathBuf},
};

/// API major version on the build-script side.
const API_MAJOR: u32 = 1;

#[cfg(feature = "grpc")]
const PROTO_ROOT: &str = "proto";

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=SOLTI_API_MAJOR={API_MAJOR}");

    #[cfg(feature = "grpc")]
    build_grpc()?;

    Ok(())
}

#[cfg(feature = "grpc")]
fn build_grpc() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed={PROTO_ROOT}");

    let protoc_path =
        protoc_bin_vendored::protoc_bin_path().expect("failed to get vendored protoc binary");

    unsafe {
        env::set_var("PROTOC", &protoc_path);
    }

    let major_dir = Path::new(PROTO_ROOT)
        .join("solti")
        .join("task")
        .join(format!("v{API_MAJOR}"));
    if !major_dir.is_dir() {
        return Err(format!(
            "expected proto directory '{}' for API major v{API_MAJOR}; \
             either add the tree or update API_MAJOR in build.rs and \
             solti_api_major! in lib.rs in lockstep",
            major_dir.display(),
        )
        .into());
    }

    let protos = collect_proto_files(Path::new(PROTO_ROOT))?;
    if protos.is_empty() {
        return Err(format!("no .proto files found under '{PROTO_ROOT}/'").into());
    }
    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .bytes(".solti.task.v1.OutputChunk.line")
        .compile_protos(&protos, &[PathBuf::from(PROTO_ROOT)])?;

    Ok(())
}

/// Recursively collect every `*.proto` file under `root`.
#[cfg(feature = "grpc")]
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
