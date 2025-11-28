//! Common model-level constants.
//!
//! This module contains well-known string keys used across the model layer.
//! Keeping them here avoids scattering magic strings throughout the codebase.

/// Label key used to route a task to a specific runner.
///
/// If a [`crate::CreateSpec`] contains`labels["runner-tag"] = "<value>"`,
/// the `RunnerRouter` will select only runners whose advertised labels contain the same tag.
///
/// This constant provides a single source of truth for the label key used in runner selection logic.
pub const LABEL_RUNNER_TAG: &str = "runner-tag";
