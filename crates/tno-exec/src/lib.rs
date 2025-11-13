pub mod error;
pub mod proc;
pub mod util;

pub mod prelude {
    pub use crate::error::ExecError;
    pub use crate::proc::ProcRunner;
    #[cfg(feature = "shell")]
    pub use crate::proc::ShellRunner;
}
