macro_rules! discovery_protocol_major {
    () => {
        1
    };
}

/// Current discovery protocol major version.
pub const DISCOVERY_PROTOCOL_VERSION: u32 = discovery_protocol_major!();

/// HTTP path used by the current discovery protocol.
pub const DISCOVERY_HTTP_SYNC_PATH: &str =
    concat!("/api/v", discovery_protocol_major!(), "/discovery/sync");

/// gRPC package used by the current discovery protocol.
pub const DISCOVERY_GRPC_PACKAGE: &str = concat!("solti.discover.v", discovery_protocol_major!());

/// gRPC service used by the current discovery protocol.
pub const DISCOVERY_GRPC_SERVICE: &str = concat!(
    "solti.discover.v",
    discovery_protocol_major!(),
    ".DiscoverService"
);
