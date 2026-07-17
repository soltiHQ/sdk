//! Script runtime resolution.
//!
//! [`Runtime`] maps script languages to interpreter paths for subprocess `Script` mode.

use serde::{Deserialize, Serialize};

/// Script interpreter for subprocess script execution.
///
/// [`Custom`](Runtime::Custom) allows arbitrary interpreter configuration.
/// Its `flag` is part of the legacy inline-script wire shape. The built-in
/// `solti-exec` runner uses tempfile transport and therefore uses the command
/// but not this flag; custom runners may interpret the pair themselves.
///
/// ## Example
///
/// ```
/// use solti_model::Runtime;
///
/// assert_eq!(Runtime::Python.resolve(), ("python3", "-c"));
///
/// let runtime = Runtime::Custom {
///     command: "ruby".into(),
///     flag: "-e".into(),
/// };
/// assert_eq!(runtime.resolve(), ("ruby", "-e"));
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Runtime {
    /// Bash shell: resolves to `("bash", "-c")`.
    Bash,
    /// Python 3 interpreter: resolves to `("python3", "-c")`.
    Python,
    /// Node.js runtime: resolves to `("node", "-e")`.
    Node,
    /// Custom interpreter with an explicit command and legacy inline flag.
    Custom {
        /// Interpreter binary (e.g. `"ruby"`, `"/usr/bin/perl"`).
        command: String,
        /// Legacy flag that precedes an inline script body (e.g. `"-e"`).
        /// The built-in `solti-exec` tempfile transport ignores it.
        flag: String,
    },
}

impl Runtime {
    /// Resolve runtime to its configured `(command, inline_flag)` pair.
    ///
    /// The built-in `solti-exec` runner uses only `command` because it passes a
    /// temporary-file path instead of an inline script body.
    ///
    /// ```rust
    /// use solti_model::Runtime;
    /// let (cmd, flag) = Runtime::Bash.resolve();
    /// assert_eq!(cmd, "bash");
    /// assert_eq!(flag, "-c");
    /// ```
    #[inline]
    pub fn resolve(&self) -> (&str, &str) {
        match self {
            Runtime::Bash => ("bash", "-c"),
            Runtime::Node => ("node", "-e"),
            Runtime::Python => ("python3", "-c"),

            Runtime::Custom { command, flag } => (command.as_str(), flag.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bash() {
        let (cmd, flag) = Runtime::Bash.resolve();
        assert_eq!(cmd, "bash");
        assert_eq!(flag, "-c");
    }

    #[test]
    fn resolve_python() {
        let (cmd, flag) = Runtime::Python.resolve();
        assert_eq!(cmd, "python3");
        assert_eq!(flag, "-c");
    }

    #[test]
    fn resolve_node() {
        let (cmd, flag) = Runtime::Node.resolve();
        assert_eq!(cmd, "node");
        assert_eq!(flag, "-e");
    }

    #[test]
    fn resolve_custom() {
        let rt = Runtime::Custom {
            command: "ruby".into(),
            flag: "-e".into(),
        };
        let (cmd, flag) = rt.resolve();
        assert_eq!(cmd, "ruby");
        assert_eq!(flag, "-e");
    }

    #[test]
    fn serde_roundtrip_bash() {
        let rt = Runtime::Bash;
        let json = serde_json::to_string(&rt).unwrap();
        assert_eq!(json, r#""bash""#);
        let back: Runtime = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rt);
    }

    #[test]
    fn serde_roundtrip_custom() {
        let rt = Runtime::Custom {
            command: "perl".into(),
            flag: "-e".into(),
        };
        let json = serde_json::to_string(&rt).unwrap();
        let back: Runtime = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rt);
    }
}
