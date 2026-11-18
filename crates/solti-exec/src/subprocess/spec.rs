//! # Subprocess workload
//!
//! [`SubprocessMode`] selects a command or an interpreter-backed script.
//! [`SubprocessSpec`] adds environment, working directory, and exit-code policy.
//! It is the body of the `exec.solti.io/v1` `Subprocess` workload.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use std::path::{Path, PathBuf};

use solti_model::{Flag, ModelError, ModelResult, TaskEnv, WorkloadBody};

use crate::EXEC_API_VERSION;
use crate::registration::at_field as at;
use crate::subprocess::backend::validate_env_name;
use crate::subprocess::task::{reject_nul, validate_env_value};

/// Kind of the subprocess workload under [`EXEC_API_VERSION`].
pub const SUBPROCESS_KIND: &str = "Subprocess";

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
/// use solti_exec::subprocess::SubprocessMode;
///
/// let mode = SubprocessMode::Script {
///     interpreter: "bash".into(),
///     body: BASE64.encode("echo hello"),
///     args: vec![],
/// };
///
/// assert_eq!(mode.decode_body().unwrap(), "echo hello");
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub enum SubprocessMode {
    /// Direct binary execution.
    Command {
        /// Executable name or path.
        #[schemars(schema_with = "crate::schema::non_empty_string")]
        command: String,
        /// Command-line arguments.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    /// Script execution via an explicit interpreter.
    Script {
        /// Interpreter executable name or path.
        #[schemars(schema_with = "crate::schema::non_empty_string")]
        interpreter: String,
        /// Standard padded base64 script body.
        #[schemars(schema_with = "crate::schema::script_body")]
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
    /// use solti_exec::subprocess::SubprocessMode;
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
    /// use solti_exec::subprocess::SubprocessMode;
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
    /// use solti_exec::subprocess::SubprocessMode;
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

/// Specification for subprocess execution on the host.
///
/// Supports two execution strategies via [`SubprocessMode`]:
/// - command: direct binary execution;
/// - script: script body passed to an explicit interpreter.
///
/// Common fields (`env`, `cwd`, `fail_on_non_zero`) apply to both modes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub struct SubprocessSpec {
    /// Execution strategy (command or script).
    pub mode: SubprocessMode,
    /// Environment variables for the process.
    #[serde(default, skip_serializing_if = "TaskEnv::is_empty")]
    pub env: TaskEnv,
    /// Working directory.
    ///
    /// A validated task spec requires this wire-facing path to be valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Whether to treat non-zero exit codes as task failure.
    ///
    /// When enabled (default), any non-zero exit code will be reported as a failure.
    #[serde(default)]
    pub fail_on_non_zero: Flag,
}

impl SubprocessSpec {
    /// Creates a subprocess spec.
    ///
    /// `SubprocessSpec` is `#[non_exhaustive]`.
    /// Use this constructor outside the crate.
    /// Validation occurs when the runner decodes the workload body.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_exec::subprocess::{SubprocessMode, SubprocessSpec};
    /// use solti_model::{Flag, TaskEnv};
    ///
    /// let spec = SubprocessSpec::new(
    ///     SubprocessMode::Command {
    ///         command: "echo".into(),
    ///         args: vec!["hello".into()],
    ///     },
    ///     TaskEnv::default(),
    ///     None,
    ///     Flag::enabled(),
    /// );
    ///
    /// assert!(spec.fail_on_non_zero.is_enabled());
    /// ```
    pub fn new(
        mode: SubprocessMode,
        env: TaskEnv,
        cwd: Option<PathBuf>,
        fail_on_non_zero: Flag,
    ) -> Self {
        Self {
            mode,
            env,
            cwd,
            fail_on_non_zero,
        }
    }
}

impl WorkloadBody for SubprocessSpec {
    const API_VERSION: &'static str = EXEC_API_VERSION;
    const KIND: &'static str = SUBPROCESS_KIND;

    /// Validates the subprocess mode, the wire-facing cwd, and what the runner hands to the
    /// operating system.
    ///
    /// The mode must be valid. `cwd` must be valid UTF-8, since it travels as JSON text.
    /// The command or interpreter, the arguments, `cwd` and the environment values must hold
    /// no NUL byte, and each environment name must be non-empty and hold no `=` or NUL: the
    /// rules the runner applies when it builds the task, so admission refuses what the build
    /// would. These refusals name the field, as `spec.env[0].key: …`.
    fn validate(&self) -> Result<(), String> {
        self.mode.validate().map_err(|error| error.to_string())?;
        let (program_path, program, args_path, args) = match &self.mode {
            SubprocessMode::Command { command, args } => {
                ("mode.command.command", command, "mode.command.args", args)
            }
            SubprocessMode::Script {
                interpreter, args, ..
            } => (
                "mode.script.interpreter",
                interpreter,
                "mode.script.args",
                args,
            ),
        };
        at(program_path, reject_nul("subprocess command", program))?;
        for (index, arg) in args.iter().enumerate() {
            at(
                format_args!("{args_path}[{index}]"),
                reject_nul("subprocess argument", arg),
            )?;
        }
        if let Some(cwd) = &self.cwd {
            validate_wire_path("subprocess cwd", cwd)?;
            at("cwd", reject_nul("subprocess cwd", &cwd.to_string_lossy()))?;
        }
        for (index, entry) in self.env.iter().enumerate() {
            at(
                format_args!("env[{index}].key"),
                validate_env_name(entry.key()),
            )?;
            at(
                format_args!("env[{index}].value"),
                validate_env_value(entry.key(), entry.value()),
            )?;
        }
        Ok(())
    }
}

fn validate_wire_path(field: &str, path: &Path) -> Result<(), String> {
    if path.to_str().is_none() {
        return Err(format!("{field} must contain valid UTF-8"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EXEC_API_VERSION;
    use solti_model::{TaskWorkload, Workload, WorkloadBodyErrorKind, workload_schema_for};

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

    fn routed_body(body: serde_json::Value) -> Workload {
        Workload::new(EXEC_API_VERSION, SUBPROCESS_KIND, body).unwrap()
    }

    #[test]
    fn spec_validation_accepts_valid_values_and_rejects_empty_fields() {
        // `cwd` remains optional text; UTF-8 validity does not make it non-empty.
        WorkloadBody::validate(&SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".into(),
                args: Vec::new(),
            },
            TaskEnv::default(),
            Some(PathBuf::new()),
            Flag::enabled(),
        ))
        .unwrap();

        let error = routed_body(serde_json::json!({"mode": {"command": {"command": "  "}}}))
            .decode::<SubprocessSpec>()
            .unwrap_err();
        assert_eq!(error.kind(), &WorkloadBodyErrorKind::Invalid);
        assert!(
            error.message().contains("command cannot be empty"),
            "{error}"
        );
    }

    #[test]
    fn spec_validation_refuses_process_input_the_build_refuses() {
        for (body, expected) in [
            (
                serde_json::json!({
                    "mode": {"command": {"command": "echo"}},
                    "env": [{"key": "A=B", "value": "1"}],
                }),
                "spec.env[0].key: invalid environment variable name \"A=B\"",
            ),
            (
                serde_json::json!({
                    "mode": {"command": {"command": "echo"}},
                    "env": [{"key": "A", "value": "1"}, {"key": "", "value": "1"}],
                }),
                "spec.env[1].key: environment variable name cannot be empty",
            ),
            (
                serde_json::json!({
                    "mode": {"command": {"command": "echo"}},
                    "env": [{"key": "A", "value": "1\u{0}"}],
                }),
                "spec.env[0].value: environment variable \"A\" contains a NUL value",
            ),
            (
                serde_json::json!({"mode": {"command": {"command": "echo", "args": ["a", "b\u{0}"]}}}),
                "spec.mode.command.args[1]: subprocess argument contains NUL",
            ),
            (
                serde_json::json!({"mode": {"command": {"command": "ec\u{0}ho"}}}),
                "spec.mode.command.command: subprocess command contains NUL",
            ),
            (
                serde_json::json!({
                    "mode": {"script": {"interpreter": "bash", "body": "ZWNobyBoaQ==", "args": ["\u{0}"]}},
                }),
                "spec.mode.script.args[0]: subprocess argument contains NUL",
            ),
            (
                serde_json::json!({"mode": {"script": {"interpreter": "ba\u{0}sh", "body": "ZWNobyBoaQ=="}}}),
                "spec.mode.script.interpreter: subprocess command contains NUL",
            ),
            (
                serde_json::json!({"mode": {"command": {"command": "echo"}}, "cwd": "/work\u{0}"}),
                "spec.cwd: subprocess cwd contains NUL",
            ),
        ] {
            let error = routed_body(body.clone())
                .decode::<SubprocessSpec>()
                .unwrap_err();
            assert_eq!(error.kind(), &WorkloadBodyErrorKind::Invalid, "{body}");
            assert!(error.message().starts_with(expected), "{body}: {error}");
        }
    }

    #[test]
    fn spec_validation_accepts_process_input_the_build_accepts() {
        for body in [
            serde_json::json!({
                "mode": {"command": {"command": "echo", "args": ["a=b", ""]}},
                "env": [{"key": "A_B", "value": "x=y"}, {"key": "EMPTY", "value": ""}],
                "cwd": "/work",
            }),
            serde_json::json!({
                "mode": {"script": {"interpreter": "bash", "body": "ZWNobyBoaQ==", "args": ["x"]}},
                "env": [{"key": "PATH", "value": "/usr/bin:/bin"}],
            }),
        ] {
            routed_body(body.clone())
                .decode::<SubprocessSpec>()
                .unwrap_or_else(|error| panic!("{body}: {error}"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_wire_paths_are_refused_without_lossy_conversion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let invalid_path = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
        let spec = SubprocessSpec::new(
            SubprocessMode::Command {
                command: "echo".into(),
                args: Vec::new(),
            },
            TaskEnv::default(),
            Some(invalid_path),
            Flag::enabled(),
        );
        let error = WorkloadBody::validate(&spec).unwrap_err();
        assert!(error.contains("subprocess cwd"), "{error}");
        assert!(error.contains("UTF-8"), "{error}");
        assert!(serde_json::to_value(&spec).is_err());
        assert!(TaskWorkload::from_body(&spec).is_err());
    }

    #[test]
    fn constructor_builds_a_spec_with_the_expected_fields() {
        let spec = SubprocessSpec::new(
            SubprocessMode::Command {
                command: "ls".into(),
                args: vec!["-l".into()],
            },
            TaskEnv::default(),
            Some(PathBuf::from("/tmp")),
            Flag::enabled(),
        );
        assert!(matches!(spec.mode, SubprocessMode::Command { .. }));
        assert_eq!(spec.cwd, Some(PathBuf::from("/tmp")));
    }

    #[test]
    fn body_declares_exec_subprocess() {
        assert_eq!(SubprocessSpec::API_VERSION, "exec.solti.io/v1");
        assert_eq!(SubprocessSpec::KIND, "Subprocess");
        assert_eq!(SUBPROCESS_KIND, "Subprocess");
    }

    #[test]
    fn subprocess_body_json_shape_is_unchanged() {
        let command = serde_json::json!({
            "mode": {"command": {"command": "echo", "args": ["a", "b"]}},
            "env": [{"key": "A", "value": "1"}],
            "cwd": "/work",
            "failOnNonZero": false,
        });
        let decoded = routed_body(command.clone())
            .decode::<SubprocessSpec>()
            .unwrap();
        assert_eq!(
            decoded.mode,
            SubprocessMode::Command {
                command: "echo".into(),
                args: vec!["a".into(), "b".into()],
            }
        );
        assert_eq!(decoded.env, TaskEnv::single("A", "1"));
        assert_eq!(decoded.cwd, Some(PathBuf::from("/work")));
        assert!(decoded.fail_on_non_zero.is_disabled());
        assert_eq!(serde_json::to_value(&decoded).unwrap(), command);

        let script = serde_json::json!({
            "mode": {"script": {"interpreter": "bash", "body": "ZWNobyBoaQ==", "args": ["x"]}},
        });
        let decoded = routed_body(script).decode::<SubprocessSpec>().unwrap();
        assert_eq!(decoded.mode.decode_body().unwrap(), "echo hi");
        assert!(decoded.fail_on_non_zero.is_enabled());
        // Defaults and empty collections are omitted on the way out.
        let out = serde_json::to_value(&decoded).unwrap();
        assert_eq!(
            out,
            serde_json::json!({
                "mode": {"script": {"interpreter": "bash", "body": "ZWNobyBoaQ==", "args": ["x"]}},
                "failOnNonZero": true,
            })
        );
    }

    #[test]
    fn subprocess_body_refuses_unknown_fields_at_every_level() {
        for body in [
            serde_json::json!({"mode": {"command": {"command": "x"}}, "extra": 1}),
            serde_json::json!({"mode": {"command": {"command": "x", "extra": 1}}}),
            serde_json::json!({"mode": {"command": {"command": "x"}, "extra": {}}}),
        ] {
            let error = routed_body(body).decode::<SubprocessSpec>().unwrap_err();
            assert!(
                matches!(error.kind(), WorkloadBodyErrorKind::Decode { .. }),
                "{error}"
            );
        }
    }

    #[test]
    fn builtin_body_schema_has_no_unbounded_64_bit_integers() {
        let schema = workload_schema_for::<SubprocessSpec>();
        assert_eq!(
            crate::schema::testing::unbounded_integers(&schema),
            Vec::<String>::new()
        );
    }
}
