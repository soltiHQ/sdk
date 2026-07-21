//! Serializable script runtime selection.

use serde::{Deserialize, Serialize};

/// Script interpreter for subprocess script execution.
///
/// [`Custom`](Runtime::Custom) allows arbitrary interpreter configuration.
/// Its `flag` is part of the legacy inline-script wire shape. The built-in
/// `solti-exec` runner uses tempfile transport and therefore uses the command
/// but not this flag; custom runners may interpret the pair themselves.
///
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Runtime {
    /// Bash shell runtime selection.
    Bash,
    /// Python runtime selection.
    Python,
    /// Node.js runtime selection.
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

#[cfg(test)]
mod tests {
    use super::*;

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
