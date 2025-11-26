mod error;
pub use error::{ExecError, ExecResult};

pub mod r#fn;
pub use r#fn::FnRunner;

pub mod prelude {
    pub use crate::FnRunner;
    pub use crate::error::{ExecError, ExecResult};
}
