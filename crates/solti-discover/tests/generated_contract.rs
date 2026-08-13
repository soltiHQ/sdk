#![cfg(all(feature = "grpc", feature = "http"))]

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const GENERATED_ROOT: &str = "src/generated";
const PROTO_ROOT: &str = "proto";
const PROTO_SOURCE_ROOT: &str = "proto/solti";

#[test]
fn generated_rust_matches_vendored_proto() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let proto_root = root.join(PROTO_ROOT);
    let protos = collect_proto_files(&root.join(PROTO_SOURCE_ROOT))?;
    let version = format!("v{}", solti_discover::DISCOVERY_PROTOCOL_VERSION);
    assert!(
        !protos.is_empty(),
        "no vendored discovery proto files found"
    );

    let temp = tempfile::tempdir()?;
    let grpc_out = temp.path().join("grpc");
    let messages_out = temp.path().join("messages");
    fs::create_dir(&grpc_out)?;
    fs::create_dir(&messages_out)?;

    let descriptor_path = grpc_out.join("solti_discover_descriptor.bin");
    generate_prost(&protos, &proto_root, &grpc_out, &descriptor_path, true)?;
    generate_json(&descriptor_path, &grpc_out, &version)?;
    generate_prost(
        &protos,
        &proto_root,
        &messages_out,
        &messages_out.join("solti_discover_descriptor.bin"),
        false,
    )?;

    let generated_root = root.join(GENERATED_ROOT);
    assert_current(
        &grpc_out.join("solti.agent.v1.rs"),
        &generated_root.join("solti.agent.v1.rs"),
    )?;
    assert_current(
        &grpc_out.join("solti.agent.v1.serde.rs"),
        &generated_root.join("solti.agent.v1.serde.rs"),
    )?;
    assert_current(
        &grpc_out.join(format!("solti.discover.{version}.rs")),
        &generated_root.join(format!("solti.discover.{version}.rs")),
    )?;
    assert_current(
        &messages_out.join(format!("solti.discover.{version}.rs")),
        &generated_root.join(format!("solti.discover.{version}.messages.rs")),
    )?;
    assert_current(
        &grpc_out.join(format!("solti.discover.{version}.serde.rs")),
        &generated_root.join(format!("solti.discover.{version}.serde.rs")),
    )?;

    Ok(())
}

fn generate_prost(
    protos: &[PathBuf],
    proto_root: &Path,
    out_dir: &Path,
    descriptor_path: &Path,
    build_client: bool,
) -> Result<(), Box<dyn Error>> {
    let mut prost = tonic_prost_build::Config::new();
    prost.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);

    tonic_prost_build::configure()
        .build_server(false)
        .build_client(build_client)
        .emit_rerun_if_changed(false)
        .out_dir(out_dir)
        .file_descriptor_set_path(descriptor_path)
        .compile_with_config(prost, protos, &[proto_root.to_path_buf()])?;
    Ok(())
}

fn generate_json(
    descriptor_path: &Path,
    out_dir: &Path,
    version: &str,
) -> Result<(), Box<dyn Error>> {
    let descriptor_set = fs::read(descriptor_path)?;
    let discover_package = format!(".solti.discover.{version}");
    pbjson_build::Builder::new()
        .out_dir(out_dir)
        .register_descriptors(&descriptor_set)?
        .build(&[".solti.agent.v1", discover_package.as_str()])?;
    Ok(())
}

fn assert_current(actual: &Path, committed: &Path) -> Result<(), Box<dyn Error>> {
    if fs::read(actual)? != fs::read(committed)? {
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
