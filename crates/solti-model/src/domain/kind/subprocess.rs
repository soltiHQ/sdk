//! # Subprocess execution specification.
//!
//! [`SubprocessSpec`] and [`SubprocessMode`] define how OS subprocess tasks are configured and validated.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::Runtime;
use crate::error::{ModelError, ModelResult};

/// Maximum script body size (after base64 decode) accepted by the model.
pub const MAX_SCRIPT_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Execution strategy for a subprocess task.
///
/// | Variant   | What it does                                                               |
/// |-----------|----------------------------------------------------------------------------|
/// | `Command` | Direct binary execution via `execve(command, args)`                        |
/// | `Script`  | Script passed to an interpreter: `execve(runtime, [flag, body, ...args])`  |
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    /// Script execution via a runtime interpreter.
    Script {
        /// Interpreter used to execute the script.
        runtime: Runtime,
        /// Base64-encoded (standard alphabet) script body.
        body: String,
        /// Additional arguments passed after the script body.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
}

impl SubprocessMode {
    /// Decode the base64 script body to a UTF-8 string, capped at the default [`MAX_SCRIPT_BODY_BYTES`].
    ///
    /// Returns `Ok(body)` for the `Script` variant; `Err` for `Command`, an invalid body, or a decoded body larger than the default cap.
    /// To use a different cap, call [`decode_body_with_limit`](Self::decode_body_with_limit).
    pub fn decode_body(&self) -> ModelResult<String> {
        self.decode_body_with_limit(MAX_SCRIPT_BODY_BYTES)
    }

    /// Decode the base64 script body to a UTF-8 string, rejecting anybody whose decoded size exceeds `max_bytes`.
    ///
    /// [`decode_body`](Self::decode_body) applies the default [`MAX_SCRIPT_BODY_BYTES`];
    /// pass an explicit `max_bytes` to tighten or relax the cap where needed.
    pub fn decode_body_with_limit(&self, max_bytes: usize) -> ModelResult<String> {
        match self {
            SubprocessMode::Command { .. } => Err(ModelError::Invalid(
                "decode_body called on Command mode".into(),
            )),
            SubprocessMode::Script { body, .. } => {
                if body.is_empty() {
                    return Err(ModelError::Invalid("script body cannot be empty".into()));
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
    /// - `Script`: body must not be empty, must be valid base64, must decode to UTF-8.
    /// - `Script` + `Custom` runtime: command and flag must not be empty.
    pub fn validate(&self) -> ModelResult<()> {
        match self {
            SubprocessMode::Command { command, .. } => {
                if command.trim().is_empty() {
                    return Err(ModelError::Invalid(
                        "subprocess command cannot be empty".into(),
                    ));
                }
            }
            SubprocessMode::Script { runtime, body, .. } => {
                if body.is_empty() {
                    return Err(ModelError::Invalid("script body cannot be empty".into()));
                }
                let bytes = BASE64
                    .decode(body)
                    .map_err(|e| ModelError::Invalid(format!("invalid base64 body: {e}").into()))?;
                if bytes.len() > MAX_SCRIPT_BODY_BYTES {
                    return Err(ModelError::Invalid(
                        format!(
                            "script body is {} bytes (decoded), maximum allowed is {} bytes",
                            bytes.len(),
                            MAX_SCRIPT_BODY_BYTES
                        )
                        .into(),
                    ));
                }
                std::str::from_utf8(&bytes).map_err(|e| {
                    ModelError::Invalid(format!("script body is not valid UTF-8: {e}").into())
                })?;

                if let Runtime::Custom { command, flag } = runtime {
                    if command.trim().is_empty() {
                        return Err(ModelError::Invalid(
                            "custom runtime command cannot be empty".into(),
                        ));
                    }
                    if flag.trim().is_empty() {
                        return Err(ModelError::Invalid(
                            "custom runtime flag cannot be empty".into(),
                        ));
                    }
                }
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
            runtime: Runtime::Bash,
            body: encode("echo hello"),
            args: vec![],
        };
        assert!(mode.validate().is_ok());
    }

    #[test]
    fn script_empty_body_fails() {
        let mode = SubprocessMode::Script {
            runtime: Runtime::Bash,
            body: "".into(),
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(err.to_string().contains("body cannot be empty"));
    }

    #[test]
    fn script_invalid_base64_fails() {
        let mode = SubprocessMode::Script {
            runtime: Runtime::Bash,
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
            runtime: Runtime::Bash,
            body: non_utf8,
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"));
    }

    #[test]
    fn script_custom_runtime_valid() {
        let mode = SubprocessMode::Script {
            runtime: Runtime::Custom {
                command: "ruby".into(),
                flag: "-e".into(),
            },
            body: encode("puts 'hello'"),
            args: vec![],
        };
        assert!(mode.validate().is_ok());
    }

    #[test]
    fn script_custom_empty_command_fails() {
        let mode = SubprocessMode::Script {
            runtime: Runtime::Custom {
                command: "".into(),
                flag: "-e".into(),
            },
            body: encode("puts 'hello'"),
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("custom runtime command cannot be empty")
        );
    }

    #[test]
    fn script_custom_empty_flag_fails() {
        let mode = SubprocessMode::Script {
            runtime: Runtime::Custom {
                command: "ruby".into(),
                flag: "".into(),
            },
            body: encode("puts 'hello'"),
            args: vec![],
        };
        let err = mode.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("custom runtime flag cannot be empty")
        );
    }

    #[test]
    fn decode_body_returns_script() {
        let mode = SubprocessMode::Script {
            runtime: Runtime::Bash,
            body: encode("echo hello"),
            args: vec![],
        };
        assert_eq!(mode.decode_body().unwrap(), "echo hello");
    }

    #[test]
    fn decode_body_with_limit_rejects_over_limit() {
        let mode = SubprocessMode::Script {
            runtime: Runtime::Bash,
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
            runtime: Runtime::Bash,
            body: encode("echo hi"),
            args: vec![],
        };
        assert_eq!(mode.decode_body_with_limit(64).unwrap(), "echo hi");
    }

    #[test]
    fn decode_body_enforces_the_default_cap() {
        let payload = "a".repeat(MAX_SCRIPT_BODY_BYTES + 1);
        let mode = SubprocessMode::Script {
            runtime: Runtime::Bash,
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
            runtime: Runtime::Bash,
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
            runtime: Runtime::Bash,
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
            runtime: Runtime::Python,
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
