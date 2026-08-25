use std::error::Error;

#[cfg(feature = "grpc")]
use std::{
    env,
    path::{Path, PathBuf},
};

/// API major version on the build-script side.
const API_MAJOR: u32 = solti_model::TASK_API_VERSION_MAJOR;

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

    let major_dir = Path::new(PROTO_ROOT)
        .join("solti")
        .join("task")
        .join(format!("v{API_MAJOR}"));
    if !major_dir.is_dir() {
        return Err(format!(
            "expected proto directory '{}' for Task API major v{API_MAJOR}; run `task proto/vendor`",
            major_dir.display(),
        )
        .into());
    }

    let protos = collect_proto_files(&major_dir)?;
    if protos.is_empty() {
        return Err(format!("no .proto files found under '{}'", major_dir.display()).into());
    }
    for proto in &protos {
        println!("cargo:rerun-if-changed={}", proto.display());
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .bytes(format!(".solti.task.v{API_MAJOR}.OutputChunk.line"))
        .emit_rerun_if_changed(false)
        .out_dir(out_dir)
        .compile_with_config(prost, &protos, &[PathBuf::from(PROTO_ROOT)])?;

    Ok(())
}

#[cfg(feature = "grpc")]
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
