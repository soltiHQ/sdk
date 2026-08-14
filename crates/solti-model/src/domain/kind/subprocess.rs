//! # Subprocess workload
//!
//! [`SubprocessMode`] selects a command or an interpreter-backed script.
//! [`SubprocessSpec`](super::SubprocessSpec) adds environment, working directory, and exit-code policy.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::error::{ModelError, ModelResult};

/// Maximum decoded script body size.
pub const MAX_SCRIPT_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Returns the largest padded base64 length for `max_bytes`.
const fn max_encoded_len(max_bytes: usize) -> usize {
    max_bytes.div_ceil(3).saturating_mul(4)
}

/// Decodes a base64 script body.
///
/// Encoded size is checked before allocation.
/// Decoded size is limited by `max_bytes`.
fn decode_script_body(body: &str, max_bytes: usize) -> ModelResult<Vec<u8>> {
    if body.is_empty() {
        return Err(ModelError::Invalid("script body cannot be empty".into()));
    }
    if body.len() > max_encoded_len(max_bytes) {
        return Err(ModelError::Invalid(
            format!(
                "script body is {} bytes (base64-encoded), maximum allowed is {} bytes (decoded)",
                body.len(),
                max_bytes
            )
            .into(),
        ));
    }
    let bytes = BASE64
        .decode(body)
        .map_err(|e| ModelError::Invalid(format!("invalid base64 body: {e}").into()))?;
    if bytes.len() > max_bytes {
        return Err(ModelError::Invalid(
            format!(
                "script body is {} bytes (decoded), maximum allowed is {} bytes",
                bytes.len(),
                max_bytes
            )
            .into(),
        ));
    }
    Ok(bytes)
}

/// Execution strategy for a subprocess task.
///
/// | Variant   | Fields                              |
/// |-----------|-------------------------------------|
/// | `Command` | executable and arguments            |
/// | `Script`  | interpreter, base64 body, arguments |
///
/// ## Example
///
/// ```
/// use base64::Engine;
/// use base64::engine::general_purpose::STANDARD as BASE64;
/// use solti_model::SubprocessMode;
///
/// let mode = SubprocessMode::Script {
///     interpreter: "bash".into(),
///     body: BASE64.encode("echo hello"),
///     args: vec![],
/// };
///
/// assert_eq!(mode.decode_body().unwrap(), "echo hello");
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub enum SubprocessMode {
    /// Direct binary execution.
    Command {
        /// Executable name or path.
        #[cfg_attr(
            feature = "schema",
            schemars(schema_with = "crate::schema::non_empty_string")
        )]
        command: String,
        /// Command-line arguments.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    /// Script execution via an explicit interpreter.
    Script {
        /// Interpreter executable name or path.
        #[cfg_attr(
            feature = "schema",
            schemars(schema_with = "crate::schema::non_empty_string")
        )]
        interpreter: String,
        /// Standard padded base64 script body.
        #[cfg_attr(
            feature = "schema",
            schemars(schema_with = "crate::schema::script_body")
        )]
        body: String,
        /// Additional arguments passed after the script body.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
}

impl SubprocessMode {
    /// Decodes the script body as UTF-8.
    ///
    /// Uses [`MAX_SCRIPT_BODY_BYTES`] as the decoded size limit.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] for command mode, invalid base64, non-UTF-8 data, an empty body, or an oversized body.
    ///
    /// ## Example
    ///
    /// ```
    /// use base64::Engine;
    /// use base64::engine::general_purpose::STANDARD as BASE64;
    /// use solti_model::SubprocessMode;
    ///
    /// let mode = SubprocessMode::Script {
    ///     interpreter: "bash".into(),
    ///     body: BASE64.encode("echo hello"),
    ///     args: vec![],
    /// };
    ///
    /// assert_eq!(mode.decode_body().unwrap(), "echo hello");
    /// ```
    pub fn decode_body(&self) -> ModelResult<String> {
        self.decode_body_with_limit(MAX_SCRIPT_BODY_BYTES)
    }

    /// Decodes the script body with a custom size limit.
    ///
    /// Encoded size is checked before allocation.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] for command mode, invalid base64, non-UTF-8 data, an empty body, or a body larger than `max_bytes`.
    ///
    /// ## Example
    ///
    /// ```
    /// use base64::Engine;
    /// use base64::engine::general_purpose::STANDARD as BASE64;
    /// use solti_model::SubprocessMode;
    ///
    /// let mode = SubprocessMode::Script {
    ///     interpreter: "bash".into(),
    ///     body: BASE64.encode("echo hello"),
    ///     args: vec![],
    /// };
    ///
    /// assert!(mode.decode_body_with_limit(4).is_err());
    /// ```
    pub fn decode_body_with_limit(&self, max_bytes: usize) -> ModelResult<String> {
        match self {
            SubprocessMode::Command { .. } => Err(ModelError::Invalid(
                "decode_body called on Command mode".into(),
            )),
            SubprocessMode::Script { body, .. } => {
                let bytes = decode_script_body(body, max_bytes)?;
                String::from_utf8(bytes).map_err(|e| {
                    ModelError::Invalid(format!("script body is not valid UTF-8: {e}").into())
                })
            }
        }
    }

    /// Validates the subprocess mode.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] for an empty command or interpreter, or for an invalid, non-UTF-8, or oversized script body.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::SubprocessMode;
    ///
    /// let mode = SubprocessMode::Command {
    ///     command: "echo".into(),
    ///     args: vec!["hello".into()],
    /// };
    ///
    /// mode.validate().unwrap();
    /// ```
    pub fn validate(&self) -> ModelResult<()> {
        match self {
            SubprocessMode::Command { command, .. } => {
                if command.trim().is_empty() {
                    return Err(ModelError::Invalid(
                        "subprocess command cannot be empty".into(),
                    ));
                }
            }
            SubprocessMode::Script {
                interpreter, body, ..
            } => {
                if interpreter.trim().is_empty() {
                    return Err(ModelError::Invalid(
                        "script interpreter cannot be empty".into(),
                    ));
                }
                let bytes = decode_script_body(body, MAX_SCRIPT_BODY_BYTES)?;
                std::str::from_utf8(&bytes).map_err(|e| {
                    ModelError::Invalid(format!("script body is not valid UTF-8: {e}").into())
                })?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(s: &str) -> String {
        BASE64.encode(s.as_bytes())
    }

    fn script(interpreter: &str, body: String) -> SubprocessMode {
        SubprocessMode::Script {
            interpreter: interpreter.into(),
            body,
            args: vec![],
        }
    }

    #[test]
    fn command_validation_accepts_content_and_rejects_empty_values() {
        SubprocessMode::Command {
            command: "ls".into(),
            args: vec!["-la".into()],
        }
        .validate()
        .unwrap();

        for command in ["", "   "] {
            let mode = SubprocessMode::Command {
                command: command.into(),
                args: vec![],
            };
            let error = mode.validate().unwrap_err();
            assert!(
                error.to_string().contains("command cannot be empty"),
                "got: {error}"
            );
        }
    }

    #[test]
    fn script_validation_accepts_interpreters_and_rejects_invalid_fields() {
        script("bash", encode("echo hello")).validate().unwrap();
        script("ruby", encode("puts 'hello'")).validate().unwrap();

        for (mode, expected) in [
            (
                script("", encode("echo hello")),
                "interpreter cannot be empty",
            ),
            (script("bash", String::new()), "body cannot be empty"),
            (
                script("bash", "not-valid-base64!!!".into()),
                "invalid base64",
            ),
            (
                script("bash", BASE64.encode([0xFF, 0xFE, 0x80])),
                "not valid UTF-8",
            ),
        ] {
            let error = mode.validate().unwrap_err();
            assert!(error.to_string().contains(expected), "got: {error}");
        }
    }

    #[test]
    fn decode_body_returns_script_and_rejects_command_mode() {
        assert_eq!(
            script("bash", encode("echo hello")).decode_body().unwrap(),
            "echo hello"
        );

        let mode = SubprocessMode::Command {
            command: "ls".into(),
            args: vec![],
        };
        assert!(mode.decode_body().is_err());
    }

    #[test]
    fn configurable_decode_limit_accepts_boundary_and_rejects_overflow() {
        let exact = script("bash", encode("12345"));
        assert_eq!(exact.decode_body_with_limit(5).unwrap(), "12345");
        assert_eq!(exact.decode_body_with_limit(64).unwrap(), "12345");

        let over = script("bash", encode("123456"));
        let error = over.decode_body_with_limit(5).unwrap_err();
        assert!(
            error.to_string().contains('5') || error.to_string().contains("maximum"),
            "got: {error}"
        );
    }

    #[test]
    fn default_limit_accepts_boundary_and_rejects_overflow() {
        let payload = "a".repeat(MAX_SCRIPT_BODY_BYTES);
        let exact = script("bash", BASE64.encode(payload.as_bytes()));
        exact.validate().expect("body at the limit must pass");
        assert_eq!(exact.decode_body().unwrap().len(), MAX_SCRIPT_BODY_BYTES);

        let payload = "a".repeat(MAX_SCRIPT_BODY_BYTES + 1);
        let over = script("bash", BASE64.encode(payload.as_bytes()));
        for error in [
            over.validate().unwrap_err(),
            over.decode_body().unwrap_err(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains(&MAX_SCRIPT_BODY_BYTES.to_string()),
                "got: {error}"
            );
        }
    }

    #[test]
    fn encoded_size_precheck_has_an_exact_boundary() {
        let threshold = MAX_SCRIPT_BODY_BYTES.div_ceil(3) * 4;
        let at_threshold = script("bash", "A".repeat(threshold));
        let error = at_threshold
            .validate()
            .expect_err("decoded body over the limit must be rejected");
        assert!(
            error.to_string().contains("(decoded)"),
            "decoded-size check must reject the boundary: {error}"
        );

        let above_threshold = script("bash", "A".repeat(threshold + 1));
        let error = above_threshold
            .validate()
            .expect_err("body over the encoded threshold must be rejected");
        assert!(
            error
                .to_string()
                .contains(&MAX_SCRIPT_BODY_BYTES.to_string()),
            "got: {error}"
        );
        assert!(
            !error.to_string().contains("invalid base64"),
            "precheck must run before decoding: {error}"
        );
    }

    #[test]
    fn serde_roundtrips_command_and_script_modes() {
        for mode in [
            SubprocessMode::Command {
                command: "echo".into(),
                args: vec!["hello".into()],
            },
            SubprocessMode::Script {
                interpreter: "python3".into(),
                body: encode("print('hello')"),
                args: vec!["--verbose".into()],
            },
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<SubprocessMode>(&json).unwrap(), mode);
        }
    }

    #[test]
    fn serde_omits_empty_command_args() {
        let mode = SubprocessMode::Command {
            command: "ls".into(),
            args: vec![],
        };
        let json = serde_json::to_string(&mode).unwrap();
        assert!(!json.contains("args"));
    }
}
