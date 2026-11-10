//! Every Git operation runs the system `git` binary.

pub mod command;
pub mod mirror;

pub use command::{Credentials, Git, GitError, GitErrorKind};
