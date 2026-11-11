//! The SSH key of a host and its stored SSH host keys.

use axum::Form;
use axum::extract::Path;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::AppState;
use super::hosts::{edit_error, load_host};
use super::repos::PageContext;
use crate::db::hosts::{self, Host};
use crate::error::AppError;
use crate::git::ssh::{self, SshError};

#[derive(Debug, Deserialize)]
pub struct KeyForm {
    private_key: String,
}

fn back_to_host(id: i64, notice: &str) -> Response {
    Redirect::to(&format!("/hosts/{id}?notice={notice}")).into_response()
}

/// Encrypts the private key and stores it with its public line.
async fn store(state: &AppState, id: i64, private: &str, public: &str) -> Result<(), AppError> {
    let encrypted = state.cipher.encrypt(private).map_err(AppError::internal)?;
    hosts::set_ssh_key(&state.pool, id, Some((&encrypted, public))).await?;
    Ok(())
}

/// Stores a pasted private key, after `ssh-keygen` read its public line.
/// The inner `Err` carries the message key.
pub(super) async fn store_pasted(
    state: &AppState,
    host: &Host,
    pasted: &str,
) -> Result<Result<(), &'static str>, AppError> {
    // Pasting often turns line ends into CRLF, which OpenSSH refuses.
    let private = pasted.replace("\r\n", "\n");
    let private = private.trim();
    if private.is_empty() {
        return Ok(Err("hosts.ssh_key_missing"));
    }
    let public = match ssh::public_key(&state.config.tmp_dir(), private).await {
        Ok(public) => public,
        Err(SshError::Keygen(err)) => {
            tracing::info!(host = %host.host, %err, "pasted SSH key refused");
            return Ok(Err("hosts.ssh_key_invalid"));
        }
        Err(err) => return Err(AppError::internal(err)),
    };
    store(state, host.id, private, &public).await?;
    tracing::info!(host = %host.host, "SSH key stored");
    Ok(Ok(()))
}

/// Creates a new ed25519 key for the host. Its public line is shown on the
/// host page, to be added to the account on the server.
pub async fn generate(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    let host = load_host(&ctx, id).await?;
    let comment = format!("kehribar@{}", host.host);
    let pair = ssh::generate(&ctx.state.config.tmp_dir(), &comment)
        .await
        .map_err(AppError::internal)?;
    store(&ctx.state, id, &pair.private, &pair.public).await?;
    tracing::info!(host = %host.host, "SSH key generated");
    Ok(back_to_host(id, "ssh_saved"))
}

/// Stores a pasted private key, after `ssh-keygen` read its public line.
pub async fn paste(
    ctx: PageContext,
    Path(id): Path<i64>,
    Form(form): Form<KeyForm>,
) -> Result<Response, AppError> {
    let host = load_host(&ctx, id).await?;
    if let Err(key) = store_pasted(&ctx.state, &host, &form.private_key).await? {
        return edit_error(&ctx, host, key).await;
    }
    Ok(back_to_host(id, "ssh_saved"))
}

pub async fn remove(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    let host = load_host(&ctx, id).await?;
    hosts::set_ssh_key(&ctx.state.pool, id, None).await?;
    tracing::info!(host = %host.host, "SSH key removed");
    Ok(back_to_host(id, "ssh_removed"))
}

/// Forgets the stored host keys, so the next fetch accepts the key the
/// server shows then. For a host key that changed on purpose.
pub async fn forget_host_keys(ctx: PageContext, Path(id): Path<i64>) -> Result<Response, AppError> {
    let host = load_host(&ctx, id).await?;
    let removed = ssh::forget(&ctx.state.config.known_hosts_of(&host.host))
        .await
        .map_err(AppError::internal)?;
    tracing::info!(host = %host.host, removed, "SSH host keys forgotten");
    Ok(back_to_host(id, "host_key_forgotten"))
}
