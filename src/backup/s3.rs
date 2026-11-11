//! A small S3 client: requests are presigned with `rusty-s3` and sent with
//! `reqwest`. Large files go up in parts, so no file is held in memory.

use std::path::Path;
use std::time::Duration;

use reqwest::header::ETAG;
use reqwest::{Client, Response};
use rusty_s3::actions::{
    AbortMultipartUpload, CompleteMultipartUpload, CompleteMultipartUploadResponse,
    CreateMultipartUpload, DeleteObject, GetObject, ListObjectsV2, PutObject, UploadPart,
};
use rusty_s3::{Bucket, Credentials, S3Action as _, UrlStyle};
use tokio::io::AsyncReadExt;

use super::settings::Target;

/// Lifetime of a presigned request. One part must start within it.
const SIGNED_FOR: Duration = Duration::from_secs(3600);
/// Size of one part of a multipart upload; S3 needs at least 5 MiB.
pub const PART_SIZE: usize = 64 * 1024 * 1024;

pub struct Store {
    bucket: Bucket,
    credentials: Credentials,
    prefix: String,
    client: Client,
    part_size: usize,
}

impl Store {
    /// `part_size` is [`PART_SIZE`] except in tests.
    pub fn new(target: &Target, part_size: usize) -> Result<Self, String> {
        let endpoint = target
            .endpoint
            .parse()
            .map_err(|err| format!("invalid S3 endpoint: {err}"))?;
        let style = if target.path_style {
            UrlStyle::Path
        } else {
            UrlStyle::VirtualHost
        };
        let bucket = Bucket::new(
            endpoint,
            style,
            target.bucket.clone(),
            target.region.clone(),
        )
        .map_err(|err| format!("invalid S3 bucket: {err}"))?;
        let client = Client::builder()
            .user_agent(concat!("git-archive/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| format!("cannot create the HTTP client: {err}"))?;
        Ok(Self {
            bucket,
            credentials: Credentials::new(&target.access_key_id, &target.secret_access_key),
            prefix: target.prefix.clone(),
            client,
            part_size,
        })
    }

    /// The object key of `name` under the configured prefix.
    pub fn key(&self, name: &str) -> String {
        if self.prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.prefix)
        }
    }

    pub async fn put_bytes(&self, key: &str, body: Vec<u8>) -> Result<(), String> {
        let url = PutObject::new(&self.bucket, Some(&self.credentials), key).sign(SIGNED_FOR);
        let response = self.client.put(url).body(body).send().await;
        check(response).await.map(drop)
    }

    pub async fn get(&self, key: &str) -> Result<Vec<u8>, String> {
        let url = GetObject::new(&self.bucket, Some(&self.credentials), key).sign(SIGNED_FOR);
        let response = check(self.client.get(url).send().await).await?;
        let bytes = response.bytes().await.map_err(request_error)?;
        Ok(bytes.to_vec())
    }

    pub async fn delete(&self, key: &str) -> Result<(), String> {
        let url = DeleteObject::new(&self.bucket, Some(&self.credentials), key).sign(SIGNED_FOR);
        check(self.client.delete(url).send().await).await.map(drop)
    }

    /// Every key that starts with `prefix`, following the continuation tokens.
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>, String> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = ListObjectsV2::new(&self.bucket, Some(&self.credentials));
            action.with_prefix(prefix);
            if let Some(token) = &token {
                action.with_continuation_token(token.as_str());
            }
            let url = action.sign(SIGNED_FOR);
            let response = check(self.client.get(url).send().await).await?;
            let body = response.text().await.map_err(request_error)?;
            let page = ListObjectsV2::parse_response(&body)
                .map_err(|err| format!("invalid S3 list answer: {err}"))?;
            keys.extend(page.contents.into_iter().map(|object| object.key));
            match page.next_continuation_token {
                Some(next) => token = Some(next),
                None => return Ok(keys),
            }
        }
    }

    /// Uploads the file at `path`: in one request when it fits into one
    /// part, else as a multipart upload that is aborted when a part fails.
    pub async fn put_file(&self, key: &str, path: &Path) -> Result<(), String> {
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?
            .len();
        if size <= self.part_size as u64 {
            let body = tokio::fs::read(path)
                .await
                .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
            return self.put_bytes(key, body).await;
        }
        let url =
            CreateMultipartUpload::new(&self.bucket, Some(&self.credentials), key).sign(SIGNED_FOR);
        let response = check(self.client.post(url).send().await).await?;
        let body = response.text().await.map_err(request_error)?;
        let upload = CreateMultipartUpload::parse_response(&body)
            .map_err(|err| format!("invalid S3 multipart answer: {err}"))?;
        let upload_id = upload.upload_id();
        match self.upload_parts(key, upload_id, path).await {
            Ok(()) => Ok(()),
            Err(err) => {
                // Stored parts cost space until the upload is aborted.
                let url = AbortMultipartUpload::new(
                    &self.bucket,
                    Some(&self.credentials),
                    key,
                    upload_id,
                )
                .sign(SIGNED_FOR);
                if let Err(abort) = check(self.client.delete(url).send().await).await {
                    tracing::warn!(%key, %abort, "cannot abort the multipart upload");
                }
                Err(err)
            }
        }
    }

    async fn upload_parts(&self, key: &str, upload_id: &str, path: &Path) -> Result<(), String> {
        let read_error = |err: std::io::Error| format!("cannot read {}: {err}", path.display());
        let mut file = tokio::fs::File::open(path).await.map_err(read_error)?;
        let mut etags = Vec::new();
        for number in 1u16.. {
            let mut part = Vec::with_capacity(self.part_size);
            (&mut file)
                .take(self.part_size as u64)
                .read_to_end(&mut part)
                .await
                .map_err(read_error)?;
            if part.is_empty() {
                break;
            }
            etags.push(self.upload_part(key, upload_id, number, part).await?);
        }
        let action = CompleteMultipartUpload::new(
            &self.bucket,
            Some(&self.credentials),
            key,
            upload_id,
            etags.iter().map(String::as_str),
        );
        let url = action.sign(SIGNED_FOR);
        let response = self.client.post(url).body(action.body()).send().await;
        let body = check(response).await?.text().await.map_err(request_error)?;
        // S3 can answer 200 OK and report the error in the body.
        CompleteMultipartUploadResponse::parse(&body)
            .map(drop)
            .map_err(|err| format!("multipart upload failed: {err}: {}", excerpt(&body)))
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u16,
        body: Vec<u8>,
    ) -> Result<String, String> {
        let url = UploadPart::new(
            &self.bucket,
            Some(&self.credentials),
            key,
            number,
            upload_id,
        )
        .sign(SIGNED_FOR);
        let response = check(self.client.put(url).body(body).send().await).await?;
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| format!("S3 answered part {number} without an ETag"))?;
        Ok(etag.to_owned())
    }
}

/// Presigned URLs carry the signature; errors never show them.
fn request_error(err: reqwest::Error) -> String {
    err.without_url().to_string()
}

/// Turns a failed request or an error status into a message with the S3
/// error code and text.
async fn check(response: reqwest::Result<Response>) -> Result<Response, String> {
    let response = response.map_err(request_error)?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = match response.text().await {
        Ok(body) => excerpt(&body),
        Err(err) => format!("cannot read the answer: {}", request_error(err)),
    };
    Err(format!("S3 HTTP {}: {body}", status.as_u16()))
}

/// The `<Code>` and `<Message>` of an S3 error answer, else its start.
fn excerpt(body: &str) -> String {
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let start = body.find(&open)? + open.len();
        let end = body[start..].find('<')? + start;
        Some(body[start..end].to_owned())
    };
    match (tag("Code"), tag("Message")) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code,
        _ => body.chars().take(200).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_names_the_s3_error() {
        let body = "<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code>\
                    <Message>Access Denied.</Message></Error>";
        assert_eq!(excerpt(body), "AccessDenied: Access Denied.");
        assert_eq!(excerpt("plain"), "plain");
    }
}
