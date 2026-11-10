//! git-archive mirrors Git repositories with their full history and serves
//! a read-only web UI for browsing the archived copies.

// Declared first so the macro is visible in every module below.
#[macro_use]
mod text_enum;

pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod i18n;
pub mod web;
