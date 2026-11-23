//! # Task workloads
//!
//! [`TaskWorkload`] is the desired state a task runs.
//!
//! Every workload uses a Kubernetes-style envelope:
//!
//! ```text
//! apiVersion
//! kind
//! spec
//! ```
//!
//! A [`Workload`] is routed: its GVK picks the runner and its `spec` is an
//! application-owned JSON object that the agent's declared schema validates.
//! [`EmbeddedSpec`] is the one SDK-internal shape, never routed.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::body::{WorkloadBody, WorkloadBodyError, WorkloadBodyErrorKind};
use crate::{ModelError, ModelResult, WorkloadTypeMeta, validation};

/// API group and version of the SDK-internal [`TaskWorkload::Embedded`] workload.
pub const EMBEDDED_WORKLOAD_API_VERSION: &str = "solti.io/v1";

/// Kind of the SDK-internal [`TaskWorkload::Embedded`] workload.
pub const EMBEDDED_WORKLOAD_KIND: &str = "Embedded";

/// Deepest nesting a workload body may have: the body object is level one and a scalar leaf counts as a level.
pub const MAX_WORKLOAD_BODY_DEPTH: usize = 128;

const RESERVED_GROUP: &str = "solti.io";

/// The refusal text for a workload type in the reserved `solti.io` group.
pub(crate) const RESERVED_GROUP_REASON: &str =
    "API group solti.io is reserved for SDK-internal workloads";

/// Whether `api_version` (`<group>/<version>`) is in the reserved `solti.io` group.
pub(crate) fn in_reserved_group(api_version: &str) -> bool {
    api_version.split('/').next() == Some(RESERVED_GROUP)
}

/// Desired state of an embedded task implementation.
///
/// The revision participates in desired-state comparison.
/// The runtime task handle is supplied separately by a higher layer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(deny_unknown_fields))]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedSpec {
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::non_empty_string")
    )]
    revision: String,
}

impl<'de> Deserialize<'de> for EmbeddedSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawEmbeddedSpec {
            revision: String,
        }

        let raw = RawEmbeddedSpec::deserialize(deserializer)?;
        Self::new(raw.revision).map_err(serde::de::Error::custom)
    }
}

impl EmbeddedSpec {
    /// Creates an embedded workload spec.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when `revision` is empty.
    pub fn new(revision: impl Into<String>) -> ModelResult<Self> {
        let spec = Self {
            revision: revision.into(),
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Caller-owned implementation revision.
    #[inline]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    fn validate(&self) -> ModelResult<()> {
        if self.revision.trim().is_empty() {
            return Err(ModelError::Invalid(
                "embedded workload revision must not be empty".into(),
            ));
        }
        Ok(())
    }
}

/// GVK envelope of a routed workload.
///
/// `spec` is a JSON object owned by the application.
/// The group `solti.io` is reserved for SDK-internal workloads.
///
/// ## Example
///
/// ```
/// use solti_model::Workload;
///
/// let workload = Workload::new(
///     "tasks.example.io/v1",
///     "Resize",
///     serde_json::json!({ "width": 320 }),
/// )
/// .unwrap();
///
/// assert_eq!(workload.kind(), "Resize");
/// assert_eq!(workload.body()["width"], 320);
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(deny_unknown_fields))]
#[cfg_attr(
    feature = "schema",
    schemars(
        description = "GVK envelope of a routed workload. spec is a JSON object owned by the application; the solti.io group is reserved for SDK-internal workloads."
    )
)]
#[serde(rename_all = "camelCase")]
pub struct Workload {
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::routed_api_version")
    )]
    api_version: String,
    kind: String,
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::json_object")
    )]
    spec: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawWorkload {
    api_version: String,
    kind: String,
    spec: Value,
}

impl<'de> Deserialize<'de> for Workload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawWorkload::deserialize(deserializer)?;
        Self::new(raw.api_version, raw.kind, raw.spec).map_err(serde::de::Error::custom)
    }
}

impl Workload {
    /// Creates a routed workload.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] for an invalid CRD GVK, the reserved
    /// `solti.io` API group, a body that is not a JSON object, or a body nested
    /// deeper than [`MAX_WORKLOAD_BODY_DEPTH`].
    pub fn new(
        api_version: impl Into<String>,
        kind: impl Into<String>,
        body: Value,
    ) -> ModelResult<Self> {
        let api_version = api_version.into();
        let kind = kind.into();
        let group = validation::validate_crd_api_version("workload apiVersion", &api_version)?;
        validation::validate_crd_kind("workload kind", &kind)?;
        if group == RESERVED_GROUP {
            return Err(ModelError::Invalid(
                format!("workload {api_version}/{kind}: {RESERVED_GROUP_REASON}").into(),
            ));
        }
        if !body.is_object() {
            return Err(ModelError::Invalid(
                format!("workload {api_version}/{kind}: spec must be a JSON object").into(),
            ));
        }
        validate_body_depth(&body, &api_version, &kind)?;
        Ok(Self {
            api_version,
            kind,
            spec: body,
        })
    }

    /// Creates a routed workload from a typed body, under the body type's GVK.
    ///
    /// The body's own [`WorkloadBody::validate`] is not run here.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the body does not serialize to a
    /// JSON object or the body type's GVK is refused by [`Workload::new`].
    pub fn from_body<B>(body: &B) -> ModelResult<Self>
    where
        B: WorkloadBody + Serialize,
    {
        let value = serde_json::to_value(body).map_err(|error| {
            ModelError::Invalid(
                format!(
                    "workload {}/{}: spec does not serialize: {error}",
                    B::API_VERSION,
                    B::KIND
                )
                .into(),
            )
        })?;
        Self::new(B::API_VERSION, B::KIND, value)
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

    /// Returns owned workload type metadata.
    pub fn type_meta(&self) -> WorkloadTypeMeta {
        WorkloadTypeMeta {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
        }
    }

    /// Application-owned desired state.
    #[inline]
    pub fn body(&self) -> &Value {
        &self.spec
    }

    /// Splits the envelope into its API version, kind and body.
    pub fn into_parts(self) -> (String, String, Value) {
        (self.api_version, self.kind, self.spec)
    }

    /// Whether the workload carries the GVK of body type `B`.
    #[inline]
    pub fn is<B: WorkloadBody>(&self) -> bool {
        self.api_version == B::API_VERSION && self.kind == B::KIND
    }

    /// Decodes the body into `B`.
    ///
    /// The GVK is checked first, then the body is deserialized with the path
    /// of a failing field tracked, then [`WorkloadBody::validate`] runs.
    ///
    /// # Errors
    ///
    /// Returns [`WorkloadBodyError`] for another GVK, a body that does not
    /// deserialize, or a body its own `validate` refuses.
    ///
    /// ## Example
    ///
    /// ```
    /// use serde::Deserialize;
    /// use solti_model::{Workload, WorkloadBody};
    ///
    /// #[derive(Deserialize)]
    /// struct Resize {
    ///     width: u32,
    /// }
    ///
    /// impl WorkloadBody for Resize {
    ///     const API_VERSION: &'static str = "tasks.example.io/v1";
    ///     const KIND: &'static str = "Resize";
    /// }
    ///
    /// let workload = Workload::new(
    ///     "tasks.example.io/v1",
    ///     "Resize",
    ///     serde_json::json!({ "width": 320 }),
    /// )
    /// .unwrap();
    ///
    /// assert_eq!(workload.decode::<Resize>().unwrap().width, 320);
    /// ```
    pub fn decode<B: WorkloadBody>(&self) -> Result<B, WorkloadBodyError> {
        let error = |reason, message: String| {
            WorkloadBodyError::new(&self.api_version, &self.kind, reason, message)
        };
        if !self.is::<B>() {
            return Err(error(
                WorkloadBodyErrorKind::TypeMismatch {
                    expected_api_version: B::API_VERSION.to_owned(),
                    expected_kind: B::KIND.to_owned(),
                },
                format!("expected {}/{}", B::API_VERSION, B::KIND),
            ));
        }
        let body: B = serde_path_to_error::deserialize(&self.spec).map_err(|failure| {
            let mut path = failure.path().to_string();
            if path == "." {
                path.clear();
            }
            let message = failure.into_inner().to_string();
            // serde reports a missing field at its parent; name the field itself.
            if let Some(field) = message
                .strip_prefix("missing field `")
                .and_then(|rest| rest.strip_suffix('`'))
            {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(field);
            }
            error(WorkloadBodyErrorKind::Decode { path }, message)
        })?;
        body.validate()
            .map_err(|message| error(WorkloadBodyErrorKind::Invalid, message))?;
        Ok(body)
    }
}

fn validate_body_depth(root: &Value, api_version: &str, kind: &str) -> ModelResult<()> {
    let mut pending = vec![(root, 1_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_WORKLOAD_BODY_DEPTH {
            return Err(ModelError::Invalid(
                format!(
                    "workload {api_version}/{kind}: spec depth exceeds max {MAX_WORKLOAD_BODY_DEPTH}"
                )
                .into(),
            ));
        }
        match value {
            Value::Array(values) => {
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            Value::Object(values) => {
                pending.extend(values.values().map(|value| (value, depth + 1)));
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    Ok(())
}

/// Executable desired state nested in a [`TaskSpec`](crate::TaskSpec).
///
/// | Variant    | Meaning                                         | Routed |
/// |------------|-------------------------------------------------|--------|
/// | `Routed`   | Any declared GVK and its application-owned body | yes    |
/// | `Embedded` | In-process implementation                       | no     |
///
/// Routed workloads are matched to a runner by exact GVK.
/// `Embedded` carries no runtime task handle.
///
/// ## Example
///
/// ```
/// use solti_model::{TaskWorkload, Workload};
///
/// let workload = TaskWorkload::from(
///     Workload::new(
///         "exec.solti.io/v1",
///         "Subprocess",
///         serde_json::json!({ "mode": { "command": "echo" } }),
///     )
///     .unwrap(),
/// );
///
/// assert_eq!(workload.kind(), "Subprocess");
/// workload.validate().unwrap();
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TaskWorkload {
    /// Workload routed to a runner by its GVK.
    Routed(Workload),

    /// Code-defined task that bypasses runner routing.
    ///
    /// A higher layer binds the desired revision to an in-process task.
    Embedded(EmbeddedSpec),
}

impl From<Workload> for TaskWorkload {
    fn from(workload: Workload) -> Self {
        Self::Routed(workload)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for TaskWorkload {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TaskWorkload".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let routed = generator.subschema_for::<Workload>();
        let embedded = schemars::json_schema!({
            "type": "object",
            "additionalProperties": false,
            "required": ["apiVersion", "kind", "spec"],
            "properties": {
                "apiVersion": {
                    "type": "string",
                    "const": EMBEDDED_WORKLOAD_API_VERSION
                },
                "kind": {
                    "type": "string",
                    "const": EMBEDDED_WORKLOAD_KIND
                },
                "spec": generator.subschema_for::<EmbeddedSpec>()
            }
        });

        schemars::json_schema!({
            "description": "Kubernetes-style workload GVK and desired state.",
            "oneOf": [routed, embedded]
        })
    }
}

impl TaskWorkload {
    /// Creates a routed workload from a typed body, under the body type's GVK.
    ///
    /// # Errors
    ///
    /// See [`Workload::from_body`].
    pub fn from_body<B>(body: &B) -> ModelResult<Self>
    where
        B: WorkloadBody + Serialize,
    {
        Workload::from_body(body).map(Self::Routed)
    }

    /// Returns the workload API group and version.
    #[inline]
    pub fn api_version(&self) -> &str {
        match self {
            Self::Routed(workload) => workload.api_version(),
            Self::Embedded(_) => EMBEDDED_WORKLOAD_API_VERSION,
        }
    }

    /// Returns the workload resource kind.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{EmbeddedSpec, TaskWorkload};
    ///
    /// let embedded = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
    /// assert_eq!(embedded.kind(), "Embedded");
    /// ```
    #[inline]
    pub fn kind(&self) -> &str {
        match self {
            Self::Routed(workload) => workload.kind(),
            Self::Embedded(_) => EMBEDDED_WORKLOAD_KIND,
        }
    }

    /// Returns owned workload type metadata.
    pub fn type_meta(&self) -> WorkloadTypeMeta {
        match self {
            Self::Routed(workload) => workload.type_meta(),
            Self::Embedded(_) => WorkloadTypeMeta {
                api_version: EMBEDDED_WORKLOAD_API_VERSION.to_owned(),
                kind: EMBEDDED_WORKLOAD_KIND.to_owned(),
            },
        }
    }

    /// Validates structural constraints.
    ///
    /// A routed workload is validated when it is built, so only `Embedded`
    /// can fail here: it requires a non-empty implementation revision.
    /// The body itself is checked against the agent's declared schema, not here.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the embedded revision is empty.
    pub fn validate(&self) -> ModelResult<()> {
        match self {
            Self::Routed(_) => Ok(()),
            Self::Embedded(spec) => spec.validate(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EmbeddedEnvelope<'a> {
    api_version: &'static str,
    kind: &'static str,
    spec: &'a EmbeddedSpec,
}

impl Serialize for TaskWorkload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Routed(workload) => workload.serialize(serializer),
            Self::Embedded(spec) => EmbeddedEnvelope {
                api_version: EMBEDDED_WORKLOAD_API_VERSION,
                kind: EMBEDDED_WORKLOAD_KIND,
                spec,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for TaskWorkload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawWorkload::deserialize(deserializer)?;
        if raw.api_version == EMBEDDED_WORKLOAD_API_VERSION && raw.kind == EMBEDDED_WORKLOAD_KIND {
            let spec = serde_json::from_value(raw.spec).map_err(serde::de::Error::custom)?;
            return Ok(Self::Embedded(spec));
        }
        Workload::new(raw.api_version, raw.kind, raw.spec)
            .map(Self::Routed)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {}
