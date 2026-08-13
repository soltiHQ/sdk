#![cfg(feature = "grpc")]

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const PROTO_ROOT: &str = "proto";

#[test]
fn generated_rust_matches_vendored_proto() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let proto_root = root.join(PROTO_ROOT);
    let version = format!("v{}", solti_api::API_VERSION);
    let protos = collect_proto_files(&proto_root.join("solti").join("task").join(&version))?;
    assert!(!protos.is_empty(), "no vendored Task API proto files found");

    let temp = tempfile::tempdir()?;
    let descriptor_path = temp.path().join("solti_task_descriptor.bin");
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .bytes(format!(".solti.task.{version}.OutputChunk.line"))
        .emit_rerun_if_changed(false)
        .out_dir(temp.path())
        .file_descriptor_set_path(&descriptor_path)
        .compile_with_config(prost, &protos, &[proto_root])?;

    let binding = format!("solti.task.{version}.rs");
    let actual = temp.path().join(&binding);
    let committed = root.join("src/generated").join(binding);
    if fs::read(actual)? != fs::read(&committed)? {
        return Err(format!(
            "generated protobuf binding '{}' is stale; regenerate it from the vendored contract",
            committed.display(),
        )
        .into());
    }

    Ok(())
}

fn collect_proto_files(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() && path.extension().is_some_and(|value| value == "proto")
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}
