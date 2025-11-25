/// Shared build context passed to all runners.
#[derive(Debug, Clone)]
pub struct BuildContext {}

impl Default for BuildContext {
    fn default() -> Self {
        Self {}
    }
}
