//! Every Git operation runs the system `git` binary.

pub mod command;
pub mod http_backend;
pub mod maintenance;
pub mod mirror;
pub mod read;
pub mod ssh;

pub use command::{Access, Credentials, Git, GitError, GitErrorKind};
