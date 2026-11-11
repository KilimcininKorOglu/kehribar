//! Kehribar mirrors Git repositories with their full history and serves
//! a read-only web UI for browsing the archived copies.

// Declared first so the macro is visible in every module below.
#[macro_use]
mod text_enum;

pub mod backup;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod git;
pub mod i18n;
pub mod name_filter;
pub mod notify;
pub mod providers;
pub mod proxy;
pub mod ref_filter;
pub mod repo_url;
pub mod settings;
pub mod sync;
pub mod transfer;
pub mod web;
