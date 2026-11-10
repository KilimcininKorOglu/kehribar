//! Every Git operation runs the system `git` binary.

pub mod command;
pub mod http_backend;
pub mod mirror;
pub mod read;

pub use command::{Credentials, Git, GitError, GitErrorKind};
