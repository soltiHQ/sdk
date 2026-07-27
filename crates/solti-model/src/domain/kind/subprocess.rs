//! Subprocess execution specification.
//!
//! [`SubprocessSpec`] and [`SubprocessMode`] define how OS subprocess tasks are configured and validated.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::error::{ModelError, ModelResult};

/// Maximum script body size (after base64 decode) accepted by the model.
pub const MAX_SCRIPT_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Upper bound on the encoded length of a standard (padded) base64 string that decodes
/// to at most `max_bytes` bytes: 4 output characters per full or partial 3-byte input group.
const fn max_encoded_len(max_bytes: usize) -> usize {
    max_bytes.div_ceil(3).saturating_mul(4)
}

/// Decode a base64 script body, enforcing `max_bytes` on the decoded size.
///
/// Bodies whose encoded length already exceeds [`max_encoded_len`] are rejected up front, before any decode buffer is allocated;
/// oversized input cannot force a large allocation just to be refused.
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
/// | Variant   | What it does                                                        |
/// |-----------|---------------------------------------------------------------------|
/// | `Command` | Direct binary execution via `execve(command, args)`                 |
/// | `Script`  | Decoded script input for an explicitly selected interpreter        |
///
/// The built-in `solti-exec` runner materializes scripts into temporary files
/// and invokes `interpreter temporary_file ...args`.
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub enum SubprocessMode {
    /// Direct binary execution.
    Command {
        /// Binary to execute (e.g. `"ls"`, `"/usr/bin/python"`).
        command: String,
        /// Command-line arguments.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    /// Script execution via an explicit interpreter.
    Script {
        /// Interpreter executable name or path.
        interpreter: String,
        /// Base64-encoded (standard alphabet) script body.
        body: String,
        /// Additional arguments passed after the script body.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
}

impl SubprocessMode {
    /// Decode the base64 script body to a UTF-8 string.
    ///
    /// Returns `Ok(body)` for the `Script` variant.
    /// To use a different cap, call [`decode_body_with_limit`](Self::decode_body_with_limit).
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

    /// Decode the base64 script body with a custom decoded size limit.
    ///
    /// [`decode_body`](Self::decode_body) applies the default [`MAX_SCRIPT_BODY_BYTES`];
    /// pass an explicit `max_bytes` to tighten or relax the cap where needed.
    ///
    /// Bodies whose encoded length already exceeds the maximum possible for `max_bytes` are rejected before any decoding takes place.
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

    /// Validate the mode at the model level.
    ///
    /// Checks:
    /// - `Command`: command must not be empty.
    /// - `Script`: interpreter and body must not be empty.
    /// - `Script`: body must be valid base64 and decode to UTF-8.
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

    #[test]
    fn command_valid() {
        let mode = SubprocessMode::Command {
            command: "ls".into(),
            args: vec!["-la".into()],
        };
        assert!(mode.validate().is_ok());
    }

    #[test]
    fn command_empty_fails() {
        let mode = SubprocessMode::Command {
            command: "".into(),
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(err.to_string().contains("command cannot be empty"));
    }

    #[test]
    fn command_whitespace_fails() {
        let mode = SubprocessMode::Command {
            command: "   ".into(),
            args: vec![],
        };
        assert!(mode.validate().is_err());
    }

    #[test]
    fn script_valid_bash() {
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: encode("echo hello"),
            args: vec![],
        };
        assert!(mode.validate().is_ok());
    }

    #[test]
    fn script_empty_body_fails() {
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: "".into(),
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(err.to_string().contains("body cannot be empty"));
    }

    #[test]
    fn script_invalid_base64_fails() {
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: "not-valid-base64!!!".into(),
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(err.to_string().contains("invalid base64"));
    }

    #[test]
    fn script_non_utf8_body_fails() {
        let non_utf8 = BASE64.encode([0xFF, 0xFE, 0x80]);
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: non_utf8,
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn script_interpreter_valid() {
        let mode = SubprocessMode::Script {
            interpreter: "ruby".into(),
            body: encode("puts 'hello'"),
            args: vec![],
        };
        assert!(mode.validate().is_ok());
    }

    #[test]
    fn script_empty_interpreter_fails() {
        let mode = SubprocessMode::Script {
            interpreter: "".into(),
            body: encode("puts 'hello'"),
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(err.to_string().contains("interpreter cannot be empty"));
    }

    #[test]
    fn decode_body_returns_script() {
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: encode("echo hello"),
            args: vec![],
        };
        assert_eq!(mode.decode_body().unwrap(), "echo hello");
    }

    #[test]
    fn decode_body_with_limit_rejects_over_limit() {
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: encode("aaaaaaaaaa"),
            args: vec![],
        };
        let err = mode
            .decode_body_with_limit(5)
            .expect_err("decoded body over the configured limit must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains('5') || msg.contains("maximum"),
            "error should mention the limit, got: {msg}"
        );
    }

    #[test]
    fn decode_body_with_limit_accepts_within_limit() {
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: encode("echo hi"),
            args: vec![],
        };
        assert_eq!(mode.decode_body_with_limit(64).unwrap(), "echo hi");
    }

    #[test]
    fn decode_body_enforces_the_default_cap() {
        let payload = "a".repeat(MAX_SCRIPT_BODY_BYTES + 1);
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: BASE64.encode(payload.as_bytes()),
            args: vec![],
        };
        let err = mode
            .decode_body()
            .expect_err("decode_body must enforce MAX_SCRIPT_BODY_BYTES by default");
        assert!(
            err.to_string().contains(&MAX_SCRIPT_BODY_BYTES.to_string()),
            "error should mention the default limit"
        );
    }

    #[test]
    fn decode_body_errors_on_command_mode() {
        let mode = SubprocessMode::Command {
            command: "ls".into(),
            args: vec![],
        };
        assert!(mode.decode_body().is_err());
    }

    #[test]
    fn script_body_within_limit_is_accepted() {
        let payload = "a".repeat(MAX_SCRIPT_BODY_BYTES);
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: BASE64.encode(payload.as_bytes()),
            args: vec![],
        };
        mode.validate()
            .expect("body at exactly the limit must pass");
    }

    #[test]
    fn script_body_over_limit_is_rejected() {
        let payload = "a".repeat(MAX_SCRIPT_BODY_BYTES + 1);
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: BASE64.encode(payload.as_bytes()),
            args: vec![],
        };
        let err = mode
            .validate()
            .expect_err("over-limit body must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains(&MAX_SCRIPT_BODY_BYTES.to_string()),
            "error should mention the limit, got: {msg}"
        );
    }

    #[test]
    fn encoded_body_at_threshold_reaches_decoded_size_check() {
        let threshold = MAX_SCRIPT_BODY_BYTES.div_ceil(3) * 4;
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: "A".repeat(threshold),
            args: vec![],
        };
        let err = mode
            .validate()
            .expect_err("decoded body over the limit must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("(decoded)"),
            "body at the encoded threshold must be rejected by the decoded-size check, got: {msg}"
        );
    }

    #[test]
    fn encoded_body_one_char_over_threshold_fails_before_decode() {
        let threshold = MAX_SCRIPT_BODY_BYTES.div_ceil(3) * 4;
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body: "A".repeat(threshold + 1),
            args: vec![],
        };
        let err = mode
            .validate()
            .expect_err("body over the encoded threshold must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains(&MAX_SCRIPT_BODY_BYTES.to_string()),
            "error should mention the limit, got: {msg}"
        );
        assert!(
            !msg.contains("invalid base64"),
            "the size pre-check must fire before base64 decoding, got: {msg}"
        );
    }

    #[test]
    fn huge_encoded_body_is_rejected_without_decoding() {
        let body = "A".repeat(100 * 1024 * 1024);
        let mode = SubprocessMode::Script {
            interpreter: "bash".into(),
            body,
            args: vec![],
        };
        let err = mode
            .validate()
            .expect_err("huge encoded body must be rejected");
        assert!(err.to_string().contains(&MAX_SCRIPT_BODY_BYTES.to_string()));
        let err = mode
            .decode_body()
            .expect_err("huge encoded body must be rejected");
        assert!(err.to_string().contains(&MAX_SCRIPT_BODY_BYTES.to_string()));
    }

    #[test]
    fn decode_body_with_limit_boundary() {
        let script = |s: &str| SubprocessMode::Script {
            interpreter: "bash".into(),
            body: encode(s),
            args: vec![],
        };
        assert_eq!(script("12345").decode_body_with_limit(5).unwrap(), "12345");
        script("123456")
            .decode_body_with_limit(5)
            .expect_err("body one byte over the limit must be rejected");
    }

    #[test]
    fn serde_roundtrip_command() {
        let mode = SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello".into()],
        };
        let json = serde_json::to_string(&mode).unwrap();
        let back: SubprocessMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mode);
    }

    #[test]
    fn serde_roundtrip_script() {
        let mode = SubprocessMode::Script {
            interpreter: "python3".into(),
            body: encode("print('hello')"),
            args: vec!["--verbose".into()],
        };
        let json = serde_json::to_string(&mode).unwrap();
        let back: SubprocessMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mode);
    }

    #[test]
    fn serde_command_empty_args_skipped() {
        let mode = SubprocessMode::Command {
            command: "ls".into(),
            args: vec![],
        };
        let json = serde_json::to_string(&mode).unwrap();
        assert!(!json.contains("args"));
    }
}
