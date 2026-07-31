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
            "expected proto directory '{}' for Task resource API major v{API_MAJOR}; \
             add the matching protobuf tree or update TASK_API_VERSION_MAJOR in solti-model",
            major_dir.display(),
        )
        .into());
    }

    let protos = collect_proto_files(&major_dir)?;
    if protos.is_empty() {
        return Err(format!("no .proto files found under '{PROTO_ROOT}/'").into());
    }
    for p in &protos {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    let out_dir: PathBuf = env::var("OUT_DIR")?.into();
    let descriptor_path = out_dir.join("solti_task_descriptor.bin");
    let grpc_package = format!("solti.task.v{API_MAJOR}");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .bytes(format!(".{grpc_package}.OutputChunk.line"))
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&protos, &[PathBuf::from(PROTO_ROOT)])?;

    let descriptor = std::fs::read(&descriptor_path)?;
    let pool = prost_reflect::DescriptorPool::decode(descriptor.as_slice())?;
    let service = format!("{grpc_package}.TaskService");
    if pool.get_service_by_name(&service).is_none() {
        return Err(format!("protobuf descriptor has no service '{service}'").into());
    }

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
