//! containerd image resolution.

use std::{future::Future, str::FromStr, time::Duration};

use containerd_client::{
    Client,
    services::v1::{GetImageRequest, ReadContentRequest, TransferOptions, TransferRequest},
    to_any,
    tonic::{
        Code, Request, Status,
        metadata::{Ascii, MetadataValue},
    },
    types::{
        Descriptor as ContainerdDescriptor,
        transfer::{ImageStore, OciRegistry, RegistryResolver, UnpackConfiguration},
    },
};
use oci_spec::{
    distribution::Reference,
    image::{
        Descriptor as OciDescriptor, Digest as OciDigest, DigestAlgorithm, ImageConfiguration,
        ImageIndex, ImageManifest, Platform as OciPlatform,
    },
};
use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256, Sha384, Sha512};

use super::{
    ContainerPlatform,
    config::{normalize_architecture, normalize_os},
};
use crate::container::ContainerEngineError;

const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;

const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_IMAGE_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const DOCKER_IMAGE_INDEX: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const DOCKER_IMAGE_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_IMAGE_CONFIG: &str = "application/vnd.docker.container.image.v1+json";

pub(super) struct ImageResolveRequest<'a> {
    pub(super) reference: &'a str,
    pub(super) platform: &'a ContainerPlatform,
    pub(super) snapshotter: &'a str,
    pub(super) registry_host_dir: Option<&'a str>,
    pub(super) control_timeout: Duration,
    pub(super) transfer_timeout: Duration,
}

pub(super) struct ResolvedImage {
    pub(super) reference: String,
    pub(super) configuration: ImageConfiguration,
    pub(super) chain_id: String,
}

pub(super) async fn resolve(
    client: &Client,
    namespace: &MetadataValue<Ascii>,
    request: ImageResolveRequest<'_>,
) -> Result<ResolvedImage, ContainerEngineError> {
    let reference = normalize_reference(request.reference)?;

    pull_and_unpack(client, namespace, &reference, &request).await?;

    let target = get_image_target(client, namespace, &reference, request.control_timeout).await?;
    let manifest = resolve_manifest(
        client,
        namespace,
        target,
        request.platform,
        request.control_timeout,
    )
    .await?;
    validate_config_media_type(manifest.config().media_type().as_ref())?;

    let config_descriptor = ContentDescriptor::try_from(manifest.config())?;
    let config_bytes = read_content(
        client,
        namespace,
        &config_descriptor,
        request.control_timeout,
    )
    .await?;
    let configuration: ImageConfiguration = decode_json(&config_bytes, "image configuration")?;

    validate_configuration(&configuration, request.platform)?;
    let chain_id = chain_id(configuration.rootfs().diff_ids())?;

    Ok(ResolvedImage {
        reference,
        configuration,
        chain_id,
    })
}

fn normalize_reference(reference: &str) -> Result<String, ContainerEngineError> {
    let reference = Reference::try_from(reference).map_err(|error| {
        ContainerEngineError::permanent_from("invalid container image reference", error)
    })?;
    if reference
        .repository()
        .bytes()
        .any(|byte| byte.is_ascii_uppercase())
    {
        return Err(ContainerEngineError::permanent(
            "container image repository must be lowercase",
        ));
    }
    Ok(reference.whole())
}

async fn pull_and_unpack(
    client: &Client,
    namespace: &MetadataValue<Ascii>,
    reference: &str,
    request: &ImageResolveRequest<'_>,
) -> Result<(), ContainerEngineError> {
    let platform = request.platform.as_containerd();
    let resolver = request.registry_host_dir.map(|host_dir| RegistryResolver {
        host_dir: host_dir.to_owned(),
        ..Default::default()
    });
    let source = OciRegistry {
        reference: reference.to_owned(),
        resolver,
    };
    let destination = ImageStore {
        name: reference.to_owned(),
        platforms: vec![platform.clone()],
        manifest_limit: 1,
        unpacks: vec![UnpackConfiguration {
            platform: Some(platform),
            snapshotter: request.snapshotter.to_owned(),
        }],
        ..Default::default()
    };
    let transfer = TransferRequest {
        source: Some(to_any(&source)),
        destination: Some(to_any(&destination)),
        options: Some(TransferOptions::default()),
    };

    rpc_with_timeout(
        request.transfer_timeout,
        "containerd image transfer failed",
        client.transfer().transfer(namespaced_with_timeout(
            transfer,
            namespace,
            request.transfer_timeout,
        )),
    )
    .await?;

    Ok(())
}

async fn get_image_target(
    client: &Client,
    namespace: &MetadataValue<Ascii>,
    reference: &str,
    control_timeout: Duration,
) -> Result<ContentDescriptor, ContainerEngineError> {
    let response = rpc_with_timeout(
        control_timeout,
        "containerd image lookup failed",
        client.images().get(namespaced_with_timeout(
            GetImageRequest {
                name: reference.to_owned(),
            },
            namespace,
            control_timeout,
        )),
    )
    .await?
    .into_inner();

    let image = response
        .image
        .ok_or_else(|| ContainerEngineError::retryable("containerd returned no image record"))?;
    let target = image.target.ok_or_else(|| {
        ContainerEngineError::permanent("containerd image has no target descriptor")
    })?;

    ContentDescriptor::try_from(target)
}

async fn resolve_manifest(
    client: &Client,
    namespace: &MetadataValue<Ascii>,
    target: ContentDescriptor,
    platform: &ContainerPlatform,
    control_timeout: Duration,
) -> Result<ImageManifest, ContainerEngineError> {
    let descriptor = if is_index_media_type(&target.media_type) {
        let index_bytes = read_content(client, namespace, &target, control_timeout).await?;
        let index: ImageIndex = decode_json(&index_bytes, "image index")?;
        validate_schema_version(index.schema_version(), "image index")?;
        select_manifest_descriptor(&index, platform)?
    } else if is_manifest_media_type(&target.media_type) {
        target
    } else {
        return Err(ContainerEngineError::permanent(format!(
            "unsupported image target media type: {}",
            target.media_type
        )));
    };

    let manifest_bytes = read_content(client, namespace, &descriptor, control_timeout).await?;
    let manifest: ImageManifest = decode_json(&manifest_bytes, "image manifest")?;
    validate_schema_version(manifest.schema_version(), "image manifest")?;
    Ok(manifest)
}

fn select_manifest_descriptor(
    index: &ImageIndex,
    platform: &ContainerPlatform,
) -> Result<ContentDescriptor, ContainerEngineError> {
    index
        .manifests()
        .iter()
        .find(|descriptor| {
            is_manifest_media_type(descriptor.media_type().as_ref())
                && descriptor
                    .platform()
                    .as_ref()
                    .is_some_and(|candidate| platform_matches(candidate, platform))
        })
        .ok_or_else(|| {
            ContainerEngineError::permanent(format!(
                "image has no manifest for platform {}/{}{}",
                platform.os(),
                platform.architecture(),
                if platform.variant().is_empty() {
                    String::new()
                } else {
                    format!("/{}", platform.variant())
                }
            ))
        })
        .and_then(ContentDescriptor::try_from)
}

fn platform_matches(candidate: &OciPlatform, requested: &ContainerPlatform) -> bool {
    platform_components_match(
        &candidate.os().to_string(),
        &candidate.architecture().to_string(),
        candidate.variant().as_deref().unwrap_or_default(),
        requested,
    )
}

fn platform_components_match(
    candidate_os: &str,
    candidate_architecture: &str,
    candidate_variant: &str,
    requested: &ContainerPlatform,
) -> bool {
    let (candidate_arch, candidate_variant) =
        normalize_architecture(candidate_architecture, candidate_variant);
    let (requested_arch, requested_variant) =
        normalize_architecture(requested.architecture(), requested.variant());

    normalize_os(candidate_os) == normalize_os(requested.os())
        && candidate_arch == requested_arch
        && candidate_variant == requested_variant
}

fn validate_configuration(
    configuration: &ImageConfiguration,
    platform: &ContainerPlatform,
) -> Result<(), ContainerEngineError> {
    if !platform_components_match(
        &configuration.os().to_string(),
        &configuration.architecture().to_string(),
        configuration.variant().as_deref().unwrap_or_default(),
        platform,
    ) {
        return Err(ContainerEngineError::permanent(
            "image configuration does not match the requested platform",
        ));
    }
    if configuration.rootfs().typ() != "layers" {
        return Err(ContainerEngineError::permanent(
            "image configuration rootfs type must be layers",
        ));
    }

    Ok(())
}

fn validate_schema_version(
    schema_version: u32,
    document: &str,
) -> Result<(), ContainerEngineError> {
    if schema_version != 2 {
        return Err(ContainerEngineError::permanent(format!(
            "{document} schemaVersion must be 2"
        )));
    }
    Ok(())
}

fn validate_config_media_type(media_type: &str) -> Result<(), ContainerEngineError> {
    if matches!(media_type, OCI_IMAGE_CONFIG | DOCKER_IMAGE_CONFIG) {
        return Ok(());
    }
    Err(ContainerEngineError::permanent(format!(
        "unsupported image configuration media type: {media_type}"
    )))
}

fn is_index_media_type(media_type: &str) -> bool {
    matches!(media_type, OCI_IMAGE_INDEX | DOCKER_IMAGE_INDEX)
}

fn is_manifest_media_type(media_type: &str) -> bool {
    matches!(media_type, OCI_IMAGE_MANIFEST | DOCKER_IMAGE_MANIFEST)
}

async fn read_content(
    client: &Client,
    namespace: &MetadataValue<Ascii>,
    descriptor: &ContentDescriptor,
    control_timeout: Duration,
) -> Result<Vec<u8>, ContainerEngineError> {
    let expected_size = checked_size(descriptor.size)?;
    let deadline = tokio::time::Instant::now().checked_add(control_timeout);
    let mut stream = rpc_until(
        deadline,
        "containerd content read failed",
        client
            .content()
            .max_decoding_message_size(MAX_METADATA_BYTES + 1024)
            .read(namespaced_with_timeout(
                ReadContentRequest {
                    digest: descriptor.digest.clone(),
                    offset: 0,
                    size: descriptor.size,
                },
                namespace,
                control_timeout,
            )),
    )
    .await?
    .into_inner();
    let mut bytes = Vec::with_capacity(expected_size);

    while let Some(chunk) = rpc_until(
        deadline,
        "containerd content stream failed",
        stream.message(),
    )
    .await?
    {
        let expected_offset = i64::try_from(bytes.len()).map_err(|error| {
            ContainerEngineError::permanent_from("containerd content offset overflow", error)
        })?;
        if chunk.offset != expected_offset {
            return Err(ContainerEngineError::retryable(
                "containerd returned a non-contiguous content stream",
            ));
        }
        append_bounded(&mut bytes, &chunk.data, expected_size)?;
    }

    if bytes.len() != expected_size {
        return Err(ContainerEngineError::retryable(format!(
            "containerd returned {} bytes for a {} byte descriptor",
            bytes.len(),
            expected_size
        )));
    }
    verify_digest(&descriptor.digest, &bytes)?;
    Ok(bytes)
}

fn checked_size(size: i64) -> Result<usize, ContainerEngineError> {
    let size = usize::try_from(size).map_err(|error| {
        ContainerEngineError::permanent_from("image descriptor size is invalid", error)
    })?;
    if size == 0 {
        return Err(ContainerEngineError::permanent(
            "image metadata descriptor cannot be empty",
        ));
    }
    if size > MAX_METADATA_BYTES {
        return Err(ContainerEngineError::permanent(format!(
            "image metadata descriptor exceeds the {MAX_METADATA_BYTES} byte limit"
        )));
    }
    Ok(size)
}

fn append_bounded(
    destination: &mut Vec<u8>,
    chunk: &[u8],
    expected_size: usize,
) -> Result<(), ContainerEngineError> {
    let new_len = destination
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| ContainerEngineError::permanent("containerd content length overflow"))?;
    if new_len > expected_size || new_len > MAX_METADATA_BYTES {
        return Err(ContainerEngineError::retryable(
            "containerd returned more content than the descriptor declares",
        ));
    }
    destination.extend_from_slice(chunk);
    Ok(())
}

fn decode_json<T: DeserializeOwned>(
    bytes: &[u8],
    document: &str,
) -> Result<T, ContainerEngineError> {
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(ContainerEngineError::permanent(format!(
            "{document} exceeds the {MAX_METADATA_BYTES} byte limit"
        )));
    }
    serde_json::from_slice(bytes)
        .map_err(|error| ContainerEngineError::permanent_from(format!("invalid {document}"), error))
}

fn verify_digest(digest: &str, bytes: &[u8]) -> Result<(), ContainerEngineError> {
    let digest = OciDigest::from_str(digest).map_err(|error| {
        ContainerEngineError::permanent_from("invalid image descriptor digest", error)
    })?;
    let calculated = match digest.algorithm() {
        DigestAlgorithm::Sha256 => lower_hex(&Sha256::digest(bytes)),
        DigestAlgorithm::Sha384 => lower_hex(&Sha384::digest(bytes)),
        DigestAlgorithm::Sha512 => lower_hex(&Sha512::digest(bytes)),
        algorithm => {
            return Err(ContainerEngineError::permanent(format!(
                "unsupported image descriptor digest algorithm: {algorithm}"
            )));
        }
    };

    if calculated != digest.digest() {
        return Err(ContainerEngineError::permanent(
            "image descriptor digest does not match its content",
        ));
    }
    Ok(())
}

fn chain_id(diff_ids: &[String]) -> Result<String, ContainerEngineError> {
    let mut diff_ids = diff_ids.iter();
    let Some(first) = diff_ids.next() else {
        return Ok(String::new());
    };
    let mut chain = validate_diff_id(first)?;

    for diff_id in diff_ids {
        let diff_id = validate_diff_id(diff_id)?;
        chain = format!(
            "sha256:{}",
            lower_hex(&Sha256::digest(format!("{chain} {diff_id}").as_bytes()))
        );
    }
    Ok(chain)
}

fn validate_diff_id(diff_id: &str) -> Result<String, ContainerEngineError> {
    OciDigest::from_str(diff_id)
        .map(|digest| digest.to_string())
        .map_err(|error| ContainerEngineError::permanent_from("invalid image layer DiffID", error))
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub(super) fn namespaced<T>(message: T, namespace: &MetadataValue<Ascii>) -> Request<T> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("containerd-namespace", namespace.clone());
    request
}

pub(super) fn namespaced_with_timeout<T>(
    message: T,
    namespace: &MetadataValue<Ascii>,
    timeout: Duration,
) -> Request<T> {
    let mut request = namespaced(message, namespace);
    request.set_timeout(timeout);
    request
}

pub(super) fn with_timeout<T>(message: T, timeout: Duration) -> Request<T> {
    let mut request = Request::new(message);
    request.set_timeout(timeout);
    request
}

pub(super) async fn rpc_with_timeout<T, F>(
    timeout: Duration,
    reason: &'static str,
    future: F,
) -> Result<T, ContainerEngineError>
where
    F: Future<Output = Result<T, Status>>,
{
    raw_rpc_with_timeout(timeout, reason, future)
        .await
        .map_err(|status| rpc_error(reason, status))
}

pub(super) async fn raw_rpc_with_timeout<T, F>(
    timeout: Duration,
    reason: &'static str,
    future: F,
) -> Result<T, Status>
where
    F: Future<Output = Result<T, Status>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(Status::deadline_exceeded(format!(
            "{reason}: client deadline exceeded"
        ))),
    }
}

async fn rpc_until<T, F>(
    deadline: Option<tokio::time::Instant>,
    reason: &'static str,
    future: F,
) -> Result<T, ContainerEngineError>
where
    F: Future<Output = Result<T, Status>>,
{
    let result = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|error| ContainerEngineError::retryable_from(reason, error))?,
        None => future.await,
    };
    result.map_err(|status| rpc_error(reason, status))
}

pub(super) fn rpc_error(reason: &'static str, status: Status) -> ContainerEngineError {
    match status.code() {
        Code::InvalidArgument
        | Code::NotFound
        | Code::AlreadyExists
        | Code::PermissionDenied
        | Code::Unauthenticated
        | Code::FailedPrecondition
        | Code::OutOfRange
        | Code::Unimplemented => ContainerEngineError::permanent_from(reason, status),
        _ => ContainerEngineError::retryable_from(reason, status),
    }
}

#[derive(Debug, Clone)]
struct ContentDescriptor {
    media_type: String,
    digest: String,
    size: i64,
}

impl TryFrom<ContainerdDescriptor> for ContentDescriptor {
    type Error = ContainerEngineError;

    fn try_from(descriptor: ContainerdDescriptor) -> Result<Self, Self::Error> {
        checked_size(descriptor.size)?;
        OciDigest::from_str(&descriptor.digest).map_err(|error| {
            ContainerEngineError::permanent_from("invalid image descriptor digest", error)
        })?;
        Ok(Self {
            media_type: descriptor.media_type,
            digest: descriptor.digest,
            size: descriptor.size,
        })
    }
}

impl TryFrom<&OciDescriptor> for ContentDescriptor {
    type Error = ContainerEngineError;

    fn try_from(descriptor: &OciDescriptor) -> Result<Self, Self::Error> {
        let size = i64::try_from(descriptor.size()).map_err(|error| {
            ContainerEngineError::permanent_from("image descriptor size is invalid", error)
        })?;
        checked_size(size)?;
        Ok(Self {
            media_type: descriptor.media_type().as_ref().to_owned(),
            digest: descriptor.digest().to_string(),
            size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::containerd::config::MAX_GRPC_TIMEOUT;

    const SHA256_A: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA256_B: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn requests_carry_the_selected_deadline() {
        let namespace = "solti".parse::<MetadataValue<Ascii>>().unwrap();
        let control = namespaced_with_timeout((), &namespace, Duration::from_secs(30));
        let transfer = namespaced_with_timeout((), &namespace, Duration::from_secs(10 * 60));
        let wait = namespaced((), &namespace);

        assert_eq!(
            control
                .metadata()
                .get("grpc-timeout")
                .unwrap()
                .to_str()
                .unwrap(),
            "30000000u"
        );
        assert_eq!(
            transfer
                .metadata()
                .get("grpc-timeout")
                .unwrap()
                .to_str()
                .unwrap(),
            "600000m"
        );
        assert!(wait.metadata().get("grpc-timeout").is_none());
        for request in [&control, &transfer, &wait] {
            assert_eq!(
                request.metadata().get("containerd-namespace").unwrap(),
                &namespace
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn finite_rpc_is_bounded_on_the_client() {
        for timeout in [Duration::from_secs(30), Duration::from_secs(10 * 60)] {
            let started = tokio::time::Instant::now();
            let error = rpc_with_timeout(
                timeout,
                "test RPC failed",
                std::future::pending::<Result<(), Status>>(),
            )
            .await
            .unwrap_err();

            assert_eq!(
                error.class(),
                crate::container::ContainerErrorClass::Retryable
            );
            assert_eq!(tokio::time::Instant::now() - started, timeout);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn raw_client_timeout_preserves_ambiguous_create_semantics() {
        let status = raw_rpc_with_timeout(
            Duration::from_secs(30),
            "test create failed",
            std::future::pending::<Result<(), Status>>(),
        )
        .await
        .unwrap_err();

        assert_eq!(status.code(), Code::DeadlineExceeded);
    }

    #[test]
    fn image_references_are_normalized() {
        assert_eq!(
            normalize_reference("alpine").unwrap(),
            "docker.io/library/alpine:latest"
        );
        assert_eq!(
            normalize_reference("ghcr.io/solti/agent:v1").unwrap(),
            "ghcr.io/solti/agent:v1"
        );
        assert!(normalize_reference("UPPERCASE/repository").is_err());
    }

    #[test]
    fn manifest_is_selected_by_normalized_platform() {
        let index: ImageIndex = serde_json::from_str(&format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "{OCI_IMAGE_INDEX}",
                "manifests": [
                    {{
                        "mediaType": "{OCI_IMAGE_MANIFEST}",
                        "digest": "{SHA256_A}",
                        "size": 1,
                        "platform": {{"os": "linux", "architecture": "amd64"}}
                    }},
                    {{
                        "mediaType": "{DOCKER_IMAGE_MANIFEST}",
                        "digest": "{SHA256_B}",
                        "size": 1,
                        "platform": {{"os": "linux", "architecture": "arm64", "variant": "v8"}}
                    }}
                ]
            }}"#
        ))
        .unwrap();

        let selected =
            select_manifest_descriptor(&index, &ContainerPlatform::new("linux", "aarch64", ""))
                .unwrap();
        assert_eq!(selected.digest, SHA256_B);
    }

    #[test]
    fn chain_id_uses_the_oci_recursive_digest() {
        assert_eq!(chain_id(&[]).unwrap(), "");
        assert_eq!(chain_id(&[SHA256_A.to_owned()]).unwrap(), SHA256_A);
        assert_eq!(
            chain_id(&[SHA256_A.to_owned(), SHA256_B.to_owned()]).unwrap(),
            "sha256:ccd722928bd92476ba1745586fed6e45a102504185ad88cd89e01ff116fd146c"
        );
    }

    #[test]
    fn maximum_supported_grpc_timeout_is_encoded_without_panic() {
        let namespace = "solti".parse::<MetadataValue<Ascii>>().unwrap();
        let namespaced = namespaced_with_timeout((), &namespace, MAX_GRPC_TIMEOUT);
        let plain = with_timeout((), MAX_GRPC_TIMEOUT);

        for request in [namespaced, plain] {
            assert_eq!(
                request
                    .metadata()
                    .get("grpc-timeout")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "99999999H"
            );
        }
    }

    #[test]
    fn metadata_limits_are_enforced_before_allocation() {
        assert_eq!(
            checked_size(MAX_METADATA_BYTES as i64).unwrap(),
            MAX_METADATA_BYTES
        );
        assert!(checked_size(0).is_err());
        assert!(checked_size(-1).is_err());
        assert!(checked_size(MAX_METADATA_BYTES as i64 + 1).is_err());

        let mut destination = vec![0; 2];
        assert!(append_bounded(&mut destination, &[0; 2], 3).is_err());
    }
}
