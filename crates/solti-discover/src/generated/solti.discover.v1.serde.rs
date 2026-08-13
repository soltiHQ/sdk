impl serde::Serialize for EndpointType {
    #[allow(deprecated)]
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let variant = match self {
            Self::Unspecified => "ENDPOINT_TYPE_UNSPECIFIED",
            Self::Grpc => "ENDPOINT_TYPE_GRPC",
            Self::Http => "ENDPOINT_TYPE_HTTP",
        };
        serializer.serialize_str(variant)
    }
}
impl<'de> serde::Deserialize<'de> for EndpointType {
    #[allow(deprecated)]
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "ENDPOINT_TYPE_UNSPECIFIED",
            "ENDPOINT_TYPE_GRPC",
            "ENDPOINT_TYPE_HTTP",
        ];

        struct GeneratedVisitor;

        impl serde::de::Visitor<'_> for GeneratedVisitor {
            type Value = EndpointType;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "expected one of: {:?}", &FIELDS)
            }

            fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                i32::try_from(v)
                    .ok()
                    .and_then(|x| x.try_into().ok())
                    .ok_or_else(|| {
                        serde::de::Error::invalid_value(serde::de::Unexpected::Signed(v), &self)
                    })
            }

            fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                i32::try_from(v)
                    .ok()
                    .and_then(|x| x.try_into().ok())
                    .ok_or_else(|| {
                        serde::de::Error::invalid_value(serde::de::Unexpected::Unsigned(v), &self)
                    })
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "ENDPOINT_TYPE_UNSPECIFIED" => Ok(EndpointType::Unspecified),
                    "ENDPOINT_TYPE_GRPC" => Ok(EndpointType::Grpc),
                    "ENDPOINT_TYPE_HTTP" => Ok(EndpointType::Http),
                    _ => Err(serde::de::Error::unknown_variant(value, FIELDS)),
                }
            }
        }
        deserializer.deserialize_any(GeneratedVisitor)
    }
}
impl serde::Serialize for SyncRequest {
    #[allow(deprecated)]
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut len = 0;
        if !self.id.is_empty() {
            len += 1;
        }
        if !self.name.is_empty() {
            len += 1;
        }
        if !self.endpoint.is_empty() {
            len += 1;
        }
        if self.uptime_seconds != 0 {
            len += 1;
        }
        if !self.os.is_empty() {
            len += 1;
        }
        if !self.arch.is_empty() {
            len += 1;
        }
        if !self.platform.is_empty() {
            len += 1;
        }
        if self.ts != 0 {
            len += 1;
        }
        if !self.metadata.is_empty() {
            len += 1;
        }
        if self.endpoint_type != 0 {
            len += 1;
        }
        if self.api_version != 0 {
            len += 1;
        }
        if self.heartbeat_interval_s != 0 {
            len += 1;
        }
        if self.capabilities.is_some() {
            len += 1;
        }
        let mut struct_ser = serializer.serialize_struct("solti.discover.v1.SyncRequest", len)?;
        if !self.id.is_empty() {
            struct_ser.serialize_field("id", &self.id)?;
        }
        if !self.name.is_empty() {
            struct_ser.serialize_field("name", &self.name)?;
        }
        if !self.endpoint.is_empty() {
            struct_ser.serialize_field("endpoint", &self.endpoint)?;
        }
        if self.uptime_seconds != 0 {
            #[allow(clippy::needless_borrow)]
            #[allow(clippy::needless_borrows_for_generic_args)]
            struct_ser.serialize_field("uptimeSeconds", ToString::to_string(&self.uptime_seconds).as_str())?;
        }
        if !self.os.is_empty() {
            struct_ser.serialize_field("os", &self.os)?;
        }
        if !self.arch.is_empty() {
            struct_ser.serialize_field("arch", &self.arch)?;
        }
        if !self.platform.is_empty() {
            struct_ser.serialize_field("platform", &self.platform)?;
        }
        if self.ts != 0 {
            #[allow(clippy::needless_borrow)]
            #[allow(clippy::needless_borrows_for_generic_args)]
            struct_ser.serialize_field("ts", ToString::to_string(&self.ts).as_str())?;
        }
        if !self.metadata.is_empty() {
            struct_ser.serialize_field("metadata", &self.metadata)?;
        }
        if self.endpoint_type != 0 {
            let v = EndpointType::try_from(self.endpoint_type)
                .map_err(|_| serde::ser::Error::custom(format!("Invalid variant {}", self.endpoint_type)))?;
            struct_ser.serialize_field("endpointType", &v)?;
        }
        if self.api_version != 0 {
            struct_ser.serialize_field("apiVersion", &self.api_version)?;
        }
        if self.heartbeat_interval_s != 0 {
            struct_ser.serialize_field("heartbeatIntervalS", &self.heartbeat_interval_s)?;
        }
        if let Some(v) = self.capabilities.as_ref() {
            struct_ser.serialize_field("capabilities", v)?;
        }
        struct_ser.end()
    }
}
impl<'de> serde::Deserialize<'de> for SyncRequest {
    #[allow(deprecated)]
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "id",
            "name",
            "endpoint",
            "uptime_seconds",
            "uptimeSeconds",
            "os",
            "arch",
            "platform",
            "ts",
            "metadata",
            "endpoint_type",
            "endpointType",
            "api_version",
            "apiVersion",
            "heartbeat_interval_s",
            "heartbeatIntervalS",
            "capabilities",
        ];

        #[allow(clippy::enum_variant_names)]
        enum GeneratedField {
            Id,
            Name,
            Endpoint,
            UptimeSeconds,
            Os,
            Arch,
            Platform,
            Ts,
            Metadata,
            EndpointType,
            ApiVersion,
            HeartbeatIntervalS,
            Capabilities,
        }
        impl<'de> serde::Deserialize<'de> for GeneratedField {
            fn deserialize<D>(deserializer: D) -> std::result::Result<GeneratedField, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct GeneratedVisitor;

                impl serde::de::Visitor<'_> for GeneratedVisitor {
                    type Value = GeneratedField;

                    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        write!(formatter, "expected one of: {:?}", &FIELDS)
                    }

                    #[allow(unused_variables)]
                    fn visit_str<E>(self, value: &str) -> std::result::Result<GeneratedField, E>
                    where
                        E: serde::de::Error,
                    {
                        match value {
                            "id" => Ok(GeneratedField::Id),
                            "name" => Ok(GeneratedField::Name),
                            "endpoint" => Ok(GeneratedField::Endpoint),
                            "uptimeSeconds" | "uptime_seconds" => Ok(GeneratedField::UptimeSeconds),
                            "os" => Ok(GeneratedField::Os),
                            "arch" => Ok(GeneratedField::Arch),
                            "platform" => Ok(GeneratedField::Platform),
                            "ts" => Ok(GeneratedField::Ts),
                            "metadata" => Ok(GeneratedField::Metadata),
                            "endpointType" | "endpoint_type" => Ok(GeneratedField::EndpointType),
                            "apiVersion" | "api_version" => Ok(GeneratedField::ApiVersion),
                            "heartbeatIntervalS" | "heartbeat_interval_s" => Ok(GeneratedField::HeartbeatIntervalS),
                            "capabilities" => Ok(GeneratedField::Capabilities),
                            _ => Err(serde::de::Error::unknown_field(value, FIELDS)),
                        }
                    }
                }
                deserializer.deserialize_identifier(GeneratedVisitor)
            }
        }
        struct GeneratedVisitor;
        impl<'de> serde::de::Visitor<'de> for GeneratedVisitor {
            type Value = SyncRequest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct solti.discover.v1.SyncRequest")
            }

            fn visit_map<V>(self, mut map_: V) -> std::result::Result<SyncRequest, V::Error>
                where
                    V: serde::de::MapAccess<'de>,
            {
                let mut id__ = None;
                let mut name__ = None;
                let mut endpoint__ = None;
                let mut uptime_seconds__ = None;
                let mut os__ = None;
                let mut arch__ = None;
                let mut platform__ = None;
                let mut ts__ = None;
                let mut metadata__ = None;
                let mut endpoint_type__ = None;
                let mut api_version__ = None;
                let mut heartbeat_interval_s__ = None;
                let mut capabilities__ = None;
                while let Some(k) = map_.next_key()? {
                    match k {
                        GeneratedField::Id => {
                            if id__.is_some() {
                                return Err(serde::de::Error::duplicate_field("id"));
                            }
                            id__ = Some(map_.next_value()?);
                        }
                        GeneratedField::Name => {
                            if name__.is_some() {
                                return Err(serde::de::Error::duplicate_field("name"));
                            }
                            name__ = Some(map_.next_value()?);
                        }
                        GeneratedField::Endpoint => {
                            if endpoint__.is_some() {
                                return Err(serde::de::Error::duplicate_field("endpoint"));
                            }
                            endpoint__ = Some(map_.next_value()?);
                        }
                        GeneratedField::UptimeSeconds => {
                            if uptime_seconds__.is_some() {
                                return Err(serde::de::Error::duplicate_field("uptimeSeconds"));
                            }
                            uptime_seconds__ = 
                                Some(map_.next_value::<::pbjson::private::NumberDeserialize<_>>()?.0)
                            ;
                        }
                        GeneratedField::Os => {
                            if os__.is_some() {
                                return Err(serde::de::Error::duplicate_field("os"));
                            }
                            os__ = Some(map_.next_value()?);
                        }
                        GeneratedField::Arch => {
                            if arch__.is_some() {
                                return Err(serde::de::Error::duplicate_field("arch"));
                            }
                            arch__ = Some(map_.next_value()?);
                        }
                        GeneratedField::Platform => {
                            if platform__.is_some() {
                                return Err(serde::de::Error::duplicate_field("platform"));
                            }
                            platform__ = Some(map_.next_value()?);
                        }
                        GeneratedField::Ts => {
                            if ts__.is_some() {
                                return Err(serde::de::Error::duplicate_field("ts"));
                            }
                            ts__ = 
                                Some(map_.next_value::<::pbjson::private::NumberDeserialize<_>>()?.0)
                            ;
                        }
                        GeneratedField::Metadata => {
                            if metadata__.is_some() {
                                return Err(serde::de::Error::duplicate_field("metadata"));
                            }
                            metadata__ = Some(
                                map_.next_value::<std::collections::HashMap<_, _>>()?
                            );
                        }
                        GeneratedField::EndpointType => {
                            if endpoint_type__.is_some() {
                                return Err(serde::de::Error::duplicate_field("endpointType"));
                            }
                            endpoint_type__ = Some(map_.next_value::<EndpointType>()? as i32);
                        }
                        GeneratedField::ApiVersion => {
                            if api_version__.is_some() {
                                return Err(serde::de::Error::duplicate_field("apiVersion"));
                            }
                            api_version__ = 
                                Some(map_.next_value::<::pbjson::private::NumberDeserialize<_>>()?.0)
                            ;
                        }
                        GeneratedField::HeartbeatIntervalS => {
                            if heartbeat_interval_s__.is_some() {
                                return Err(serde::de::Error::duplicate_field("heartbeatIntervalS"));
                            }
                            heartbeat_interval_s__ = 
                                Some(map_.next_value::<::pbjson::private::NumberDeserialize<_>>()?.0)
                            ;
                        }
                        GeneratedField::Capabilities => {
                            if capabilities__.is_some() {
                                return Err(serde::de::Error::duplicate_field("capabilities"));
                            }
                            capabilities__ = map_.next_value()?;
                        }
                    }
                }
                Ok(SyncRequest {
                    id: id__.unwrap_or_default(),
                    name: name__.unwrap_or_default(),
                    endpoint: endpoint__.unwrap_or_default(),
                    uptime_seconds: uptime_seconds__.unwrap_or_default(),
                    os: os__.unwrap_or_default(),
                    arch: arch__.unwrap_or_default(),
                    platform: platform__.unwrap_or_default(),
                    ts: ts__.unwrap_or_default(),
                    metadata: metadata__.unwrap_or_default(),
                    endpoint_type: endpoint_type__.unwrap_or_default(),
                    api_version: api_version__.unwrap_or_default(),
                    heartbeat_interval_s: heartbeat_interval_s__.unwrap_or_default(),
                    capabilities: capabilities__,
                })
            }
        }
        deserializer.deserialize_struct("solti.discover.v1.SyncRequest", FIELDS, GeneratedVisitor)
    }
}
impl serde::Serialize for SyncResponse {
    #[allow(deprecated)]
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut len = 0;
        if self.success {
            len += 1;
        }
        if !self.reason.is_empty() {
            len += 1;
        }
        if self.retry_after_s != 0 {
            len += 1;
        }
        let mut struct_ser = serializer.serialize_struct("solti.discover.v1.SyncResponse", len)?;
        if self.success {
            struct_ser.serialize_field("success", &self.success)?;
        }
        if !self.reason.is_empty() {
            struct_ser.serialize_field("reason", &self.reason)?;
        }
        if self.retry_after_s != 0 {
            struct_ser.serialize_field("retryAfterS", &self.retry_after_s)?;
        }
        struct_ser.end()
    }
}
impl<'de> serde::Deserialize<'de> for SyncResponse {
    #[allow(deprecated)]
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "success",
            "reason",
            "retry_after_s",
            "retryAfterS",
        ];

        #[allow(clippy::enum_variant_names)]
        enum GeneratedField {
            Success,
            Reason,
            RetryAfterS,
        }
        impl<'de> serde::Deserialize<'de> for GeneratedField {
            fn deserialize<D>(deserializer: D) -> std::result::Result<GeneratedField, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct GeneratedVisitor;

                impl serde::de::Visitor<'_> for GeneratedVisitor {
                    type Value = GeneratedField;

                    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        write!(formatter, "expected one of: {:?}", &FIELDS)
                    }

                    #[allow(unused_variables)]
                    fn visit_str<E>(self, value: &str) -> std::result::Result<GeneratedField, E>
                    where
                        E: serde::de::Error,
                    {
                        match value {
                            "success" => Ok(GeneratedField::Success),
                            "reason" => Ok(GeneratedField::Reason),
                            "retryAfterS" | "retry_after_s" => Ok(GeneratedField::RetryAfterS),
                            _ => Err(serde::de::Error::unknown_field(value, FIELDS)),
                        }
                    }
                }
                deserializer.deserialize_identifier(GeneratedVisitor)
            }
        }
        struct GeneratedVisitor;
        impl<'de> serde::de::Visitor<'de> for GeneratedVisitor {
            type Value = SyncResponse;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct solti.discover.v1.SyncResponse")
            }

            fn visit_map<V>(self, mut map_: V) -> std::result::Result<SyncResponse, V::Error>
                where
                    V: serde::de::MapAccess<'de>,
            {
                let mut success__ = None;
                let mut reason__ = None;
                let mut retry_after_s__ = None;
                while let Some(k) = map_.next_key()? {
                    match k {
                        GeneratedField::Success => {
                            if success__.is_some() {
                                return Err(serde::de::Error::duplicate_field("success"));
                            }
                            success__ = Some(map_.next_value()?);
                        }
                        GeneratedField::Reason => {
                            if reason__.is_some() {
                                return Err(serde::de::Error::duplicate_field("reason"));
                            }
                            reason__ = Some(map_.next_value()?);
                        }
                        GeneratedField::RetryAfterS => {
                            if retry_after_s__.is_some() {
                                return Err(serde::de::Error::duplicate_field("retryAfterS"));
                            }
                            retry_after_s__ = 
                                Some(map_.next_value::<::pbjson::private::NumberDeserialize<_>>()?.0)
                            ;
                        }
                    }
                }
                Ok(SyncResponse {
                    success: success__.unwrap_or_default(),
                    reason: reason__.unwrap_or_default(),
                    retry_after_s: retry_after_s__.unwrap_or_default(),
                })
            }
        }
        deserializer.deserialize_struct("solti.discover.v1.SyncResponse", FIELDS, GeneratedVisitor)
    }
}
