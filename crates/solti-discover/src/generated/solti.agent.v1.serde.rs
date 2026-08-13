impl serde::Serialize for AgentCapabilities {
    #[allow(deprecated)]
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut len = 0;
        if !self.runners.is_empty() {
            len += 1;
        }
        let mut struct_ser = serializer.serialize_struct("solti.agent.v1.AgentCapabilities", len)?;
        if !self.runners.is_empty() {
            struct_ser.serialize_field("runners", &self.runners)?;
        }
        struct_ser.end()
    }
}
impl<'de> serde::Deserialize<'de> for AgentCapabilities {
    #[allow(deprecated)]
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "runners",
        ];

        #[allow(clippy::enum_variant_names)]
        enum GeneratedField {
            Runners,
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
                            "runners" => Ok(GeneratedField::Runners),
                            _ => Err(serde::de::Error::unknown_field(value, FIELDS)),
                        }
                    }
                }
                deserializer.deserialize_identifier(GeneratedVisitor)
            }
        }
        struct GeneratedVisitor;
        impl<'de> serde::de::Visitor<'de> for GeneratedVisitor {
            type Value = AgentCapabilities;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct solti.agent.v1.AgentCapabilities")
            }

            fn visit_map<V>(self, mut map_: V) -> std::result::Result<AgentCapabilities, V::Error>
                where
                    V: serde::de::MapAccess<'de>,
            {
                let mut runners__ = None;
                while let Some(k) = map_.next_key()? {
                    match k {
                        GeneratedField::Runners => {
                            if runners__.is_some() {
                                return Err(serde::de::Error::duplicate_field("runners"));
                            }
                            runners__ = Some(map_.next_value()?);
                        }
                    }
                }
                Ok(AgentCapabilities {
                    runners: runners__.unwrap_or_default(),
                })
            }
        }
        deserializer.deserialize_struct("solti.agent.v1.AgentCapabilities", FIELDS, GeneratedVisitor)
    }
}
impl serde::Serialize for RunnerCapability {
    #[allow(deprecated)]
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut len = 0;
        if !self.name.is_empty() {
            len += 1;
        }
        if !self.labels.is_empty() {
            len += 1;
        }
        if !self.workloads.is_empty() {
            len += 1;
        }
        let mut struct_ser = serializer.serialize_struct("solti.agent.v1.RunnerCapability", len)?;
        if !self.name.is_empty() {
            struct_ser.serialize_field("name", &self.name)?;
        }
        if !self.labels.is_empty() {
            struct_ser.serialize_field("labels", &self.labels)?;
        }
        if !self.workloads.is_empty() {
            struct_ser.serialize_field("workloads", &self.workloads)?;
        }
        struct_ser.end()
    }
}
impl<'de> serde::Deserialize<'de> for RunnerCapability {
    #[allow(deprecated)]
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "name",
            "labels",
            "workloads",
        ];

        #[allow(clippy::enum_variant_names)]
        enum GeneratedField {
            Name,
            Labels,
            Workloads,
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
                            "name" => Ok(GeneratedField::Name),
                            "labels" => Ok(GeneratedField::Labels),
                            "workloads" => Ok(GeneratedField::Workloads),
                            _ => Err(serde::de::Error::unknown_field(value, FIELDS)),
                        }
                    }
                }
                deserializer.deserialize_identifier(GeneratedVisitor)
            }
        }
        struct GeneratedVisitor;
        impl<'de> serde::de::Visitor<'de> for GeneratedVisitor {
            type Value = RunnerCapability;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct solti.agent.v1.RunnerCapability")
            }

            fn visit_map<V>(self, mut map_: V) -> std::result::Result<RunnerCapability, V::Error>
                where
                    V: serde::de::MapAccess<'de>,
            {
                let mut name__ = None;
                let mut labels__ = None;
                let mut workloads__ = None;
                while let Some(k) = map_.next_key()? {
                    match k {
                        GeneratedField::Name => {
                            if name__.is_some() {
                                return Err(serde::de::Error::duplicate_field("name"));
                            }
                            name__ = Some(map_.next_value()?);
                        }
                        GeneratedField::Labels => {
                            if labels__.is_some() {
                                return Err(serde::de::Error::duplicate_field("labels"));
                            }
                            labels__ = Some(
                                map_.next_value::<std::collections::HashMap<_, _>>()?
                            );
                        }
                        GeneratedField::Workloads => {
                            if workloads__.is_some() {
                                return Err(serde::de::Error::duplicate_field("workloads"));
                            }
                            workloads__ = Some(map_.next_value()?);
                        }
                    }
                }
                Ok(RunnerCapability {
                    name: name__.unwrap_or_default(),
                    labels: labels__.unwrap_or_default(),
                    workloads: workloads__.unwrap_or_default(),
                })
            }
        }
        deserializer.deserialize_struct("solti.agent.v1.RunnerCapability", FIELDS, GeneratedVisitor)
    }
}
impl serde::Serialize for WorkloadType {
    #[allow(deprecated)]
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut len = 0;
        if !self.api_version.is_empty() {
            len += 1;
        }
        if !self.kind.is_empty() {
            len += 1;
        }
        let mut struct_ser = serializer.serialize_struct("solti.agent.v1.WorkloadType", len)?;
        if !self.api_version.is_empty() {
            struct_ser.serialize_field("apiVersion", &self.api_version)?;
        }
        if !self.kind.is_empty() {
            struct_ser.serialize_field("kind", &self.kind)?;
        }
        struct_ser.end()
    }
}
impl<'de> serde::Deserialize<'de> for WorkloadType {
    #[allow(deprecated)]
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const FIELDS: &[&str] = &[
            "api_version",
            "apiVersion",
            "kind",
        ];

        #[allow(clippy::enum_variant_names)]
        enum GeneratedField {
            ApiVersion,
            Kind,
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
                            "apiVersion" | "api_version" => Ok(GeneratedField::ApiVersion),
                            "kind" => Ok(GeneratedField::Kind),
                            _ => Err(serde::de::Error::unknown_field(value, FIELDS)),
                        }
                    }
                }
                deserializer.deserialize_identifier(GeneratedVisitor)
            }
        }
        struct GeneratedVisitor;
        impl<'de> serde::de::Visitor<'de> for GeneratedVisitor {
            type Value = WorkloadType;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("struct solti.agent.v1.WorkloadType")
            }

            fn visit_map<V>(self, mut map_: V) -> std::result::Result<WorkloadType, V::Error>
                where
                    V: serde::de::MapAccess<'de>,
            {
                let mut api_version__ = None;
                let mut kind__ = None;
                while let Some(k) = map_.next_key()? {
                    match k {
                        GeneratedField::ApiVersion => {
                            if api_version__.is_some() {
                                return Err(serde::de::Error::duplicate_field("apiVersion"));
                            }
                            api_version__ = Some(map_.next_value()?);
                        }
                        GeneratedField::Kind => {
                            if kind__.is_some() {
                                return Err(serde::de::Error::duplicate_field("kind"));
                            }
                            kind__ = Some(map_.next_value()?);
                        }
                    }
                }
                Ok(WorkloadType {
                    api_version: api_version__.unwrap_or_default(),
                    kind: kind__.unwrap_or_default(),
                })
            }
        }
        deserializer.deserialize_struct("solti.agent.v1.WorkloadType", FIELDS, GeneratedVisitor)
    }
}
