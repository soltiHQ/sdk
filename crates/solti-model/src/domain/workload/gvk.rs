//! Group/version and kind of one workload type.

use serde::{Deserialize, Deserializer, Serialize};

use crate::{ModelResult, validation};

/// Group/version and kind of one workload schema.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(deny_unknown_fields))]
#[serde(rename_all = "camelCase")]
pub struct WorkloadTypeMeta {
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::crd_api_version")
    )]
    pub(super) api_version: String,
    #[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::crd_kind"))]
    pub(super) kind: String,
}

impl<'de> Deserialize<'de> for WorkloadTypeMeta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct RawWorkloadTypeMeta {
            api_version: String,
            kind: String,
        }

        let raw = RawWorkloadTypeMeta::deserialize(deserializer)?;
        Self::new(raw.api_version, raw.kind).map_err(serde::de::Error::custom)
    }
}

impl WorkloadTypeMeta {
    /// Creates validated workload type metadata.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`](crate::ModelError::Invalid) for an invalid CRD group/version or kind.
    pub fn new(api_version: impl Into<String>, kind: impl Into<String>) -> ModelResult<Self> {
        let type_meta = Self {
            api_version: api_version.into(),
            kind: kind.into(),
        };
        type_meta.validate()?;
        Ok(type_meta)
    }

    /// Workload API group and version.
    #[inline]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Workload resource kind.
    #[inline]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    fn validate(&self) -> ModelResult<()> {
        validation::validate_crd_api_version("workload apiVersion", &self.api_version)?;
        validation::validate_crd_kind("workload kind", &self.kind)
    }
}
